//! The shared worker-registry core: a [`topology::Membership`] identity/address
//! set + a proxy-only [`WorkerAnnotations`] overlay, composed into the
//! [`WorkerEntry`] set the LB reads.
//!
//! This is "the rest" that every registry impl now shares. Before, each of the
//! three registries (`static`, `simulated`, `membership`) carried its own
//! `RegistryState` — a second container mirroring topology's `MembershipState`
//! — plus a forked copy of topology's `reconcile_to_desired`. Once **health** (the
//! one genuinely proxy-only, OPTIONS-driven field) is lifted out of the worker
//! set, the set *is* membership, and a `WorkerEntry` is a pure projection:
//!
//! ```text
//!   WorkerEntry = membership.host  ⊕  annotation{ port, health, draining, first_seen }
//! ```
//!
//! - **Identity + host** live in `topology::Membership` — one container, one diff
//!   (`reconcile_to_desired`), one self-heal loop (`spawn_membership_reconcile`),
//!   shared with the b2bua replication supervisor.
//! - **Port + health + LB-timing** live in [`WorkerAnnotations`], keyed by ordinal.
//!   Port is here (not in membership) because topology is deliberately
//!   port-agnostic — the port is a consumer concern. In production every worker
//!   uses the cluster-wide `default_port`; `static`/`simulated` may set a
//!   per-worker `port_override`.
//! - **The projection** is cached in one [`ArcSwap`] for lock-free hot reads and
//!   is a pure function of the two authoritative sources — never an independent
//!   source of truth, always re-derivable, inherently lag-immune (it recomposes
//!   from the snapshot, never replays deltas).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use sip_clock::Clock;
use topology::{Membership, Peer};

use crate::addr::ProxyAddr;

use super::{WorkerEntry, WorkerHealth, WorkerId};

/// Per-ordinal proxy annotation — everything a [`WorkerEntry`] carries that
/// membership does not. `host` is shadowed only to detect an address move (→ a
/// fresh, re-probed endpoint); routing always takes the host from membership at
/// projection time.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Annot {
    health: WorkerHealth,
    draining_since: Option<u64>,
    first_seen_at_ms: Option<u64>,
    host: String,
    /// Per-worker port. `None` → use the registry's `default_port` (the
    /// production/k8s case; every worker shares the cluster SIP port).
    port_override: Option<u16>,
}

impl Annot {
    /// A brand-new annotation for a just-(re)joined endpoint: `Unknown` (not
    /// routable until the OPTIONS probe confirms it), `first_seen` stamped so the
    /// LB fresh-pod guard engages.
    fn fresh(host: String, now_ms: u64) -> Self {
        Self { health: WorkerHealth::Unknown, draining_since: None, first_seen_at_ms: Some(now_ms), host, port_override: None }
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
    fn sync_membership(&self, peers: &[Peer], now_ms: u64) {
        let live: HashSet<&str> = peers.iter().map(|p| p.ordinal.as_str()).collect();
        self.update(|recs| {
            recs.retain(|id, _| live.contains(id.as_str()));
            for p in peers {
                match recs.get(&p.ordinal) {
                    Some(rec) if rec.host == p.host => {} // unchanged → preserve
                    _ => {
                        recs.insert(p.ordinal.clone(), Annot::fresh(p.host.clone(), now_ms));
                    }
                }
            }
        });
    }

    /// Annotate health (the OPTIONS probe write / `WorkerRegistryControl`). No-op
    /// if the worker is unknown or unchanged; entering `Draining` stamps
    /// `draining_since`.
    fn set_health(&self, id: &str, health: WorkerHealth, now_ms: u64) {
        if self.snapshot().get(id).map(|r| r.health) == Some(health) {
            return;
        }
        self.update(|recs| {
            if let Some(rec) = recs.get_mut(id) {
                rec.health = health;
                if health == WorkerHealth::Draining && rec.draining_since.is_none() {
                    rec.draining_since = Some(now_ms);
                }
            }
        });
    }
}

/// Project the membership peer set + the annotation overlay into the worker set
/// the LB reads. Pure: same inputs → same output, no diff, no stored identity.
/// `default_port` fills in workers with no `port_override` (the production case).
/// A peer with no annotation reads as `Unknown` (not routable) and self-corrects
/// on the next recompose. Duplicate ordinals are de-duped first-wins (defensive;
/// topology snapshots are already unique).
fn project(peers: &[Peer], annots: &HashMap<WorkerId, Annot>, default_port: u16) -> Vec<WorkerEntry> {
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
                draining_since: a.and_then(|a| a.draining_since),
                first_seen_at_ms: a.and_then(|a| a.first_seen_at_ms),
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
    projection: ArcSwap<Vec<WorkerEntry>>,
    default_port: u16,
    clock: Clock,
}

impl WorkerSet {
    /// Build over `membership` with an empty annotation overlay; the projection is
    /// computed once so reads are valid immediately.
    pub(crate) fn new(membership: Arc<dyn Membership>, default_port: u16, clock: Clock) -> Self {
        let set = Self {
            membership,
            annotations: WorkerAnnotations::default(),
            projection: ArcSwap::from_pointee(Vec::new()),
            default_port,
            clock,
        };
        set.recompose();
        set
    }

    fn now_ms(&self) -> u64 {
        self.clock.now_ms().max(0) as u64
    }

    /// Recompute the projection from the authoritative membership snapshot + the
    /// annotation overlay. Idempotent; the single write path for every registry.
    pub(crate) fn recompose(&self) {
        let peers = self.membership.snapshot();
        self.annotations.sync_membership(&peers, self.now_ms());
        let annots = self.annotations.snapshot();
        self.projection.store(Arc::new(project(&peers, &annots, self.default_port)));
    }

    /// Annotate a worker's health (the OPTIONS write seam) and recompose.
    pub(crate) fn set_health(&self, id: &str, health: WorkerHealth) {
        self.annotations.set_health(id, health, self.now_ms());
        self.recompose();
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
    /// the subsequent `recompose` preserves the intended port/health/timing instead
    /// of seeding a fresh `Unknown`. For the `static`/`simulated` add paths.
    pub(crate) fn preset(
        &self,
        id: &str,
        host: String,
        port: u16,
        health: WorkerHealth,
        draining_since: Option<u64>,
        first_seen_at_ms: Option<u64>,
    ) {
        self.annotations.update(|recs| {
            recs.insert(
                id.to_string(),
                Annot { health, draining_since, first_seen_at_ms, host: host.clone(), port_override: Some(port) },
            );
        });
    }

    // ---- read seam (lock-free, hot path) ----

    pub(crate) fn snapshot(&self) -> Vec<WorkerEntry> {
        self.projection.load().as_ref().clone()
    }
    pub(crate) fn resolve(&self, id: &str) -> Option<WorkerEntry> {
        self.projection.load().iter().find(|w| w.id == id).cloned()
    }
    pub(crate) fn lookup_by_address(&self, addr: &ProxyAddr) -> Option<WorkerEntry> {
        self.projection.load().iter().find(|w| &w.address == addr).cloned()
    }
    pub(crate) fn membership(&self) -> &Arc<dyn Membership> {
        &self.membership
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use topology::SimulatedMembership;

    fn ws(initial: &[(&str, &str)], port: u16) -> (WorkerSet, SimulatedMembership) {
        let sim = SimulatedMembership::with_clock(
            initial.iter().map(|(o, h)| Peer::new(*o, *h)).collect(),
            Clock::test_at(0),
        );
        let set = WorkerSet::new(Arc::new(sim.clone()), port, Clock::test_at(0));
        (set, sim)
    }

    #[test]
    fn new_workers_start_unknown_with_default_port() {
        let (set, _sim) = ws(&[("w0", "10.0.0.1")], 5060);
        let w0 = set.resolve("w0").unwrap();
        assert_eq!(w0.health, WorkerHealth::Unknown);
        assert_eq!(w0.address, ProxyAddr::new("10.0.0.1", 5060));
    }

    #[test]
    fn health_write_composes_and_survives_membership_recompose() {
        let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
        set.set_health("w0", WorkerHealth::Alive);
        assert_eq!(set.resolve("w0").unwrap().health, WorkerHealth::Alive);
        // A membership change (new peer) must not stomp w0's probe-written health.
        sim.add(Peer::new("w1", "10.0.0.2"));
        set.recompose();
        assert_eq!(set.resolve("w0").unwrap().health, WorkerHealth::Alive);
        assert_eq!(set.resolve("w1").unwrap().health, WorkerHealth::Unknown);
    }

    #[test]
    fn address_move_resets_health() {
        let (set, sim) = ws(&[("w0", "10.0.0.1")], 5060);
        set.set_health("w0", WorkerHealth::Dead);
        sim.change_address(Peer::new("w0", "10.0.0.2"));
        set.recompose();
        let w0 = set.resolve("w0").unwrap();
        assert_eq!(w0.address, ProxyAddr::new("10.0.0.2", 5060));
        assert_eq!(w0.health, WorkerHealth::Unknown, "a stale Dead must not survive a restart");
    }

    #[test]
    fn departed_worker_pruned_on_recompose() {
        let (set, sim) = ws(&[("w0", "10.0.0.1"), ("w1", "10.0.0.2")], 5060);
        sim.remove("w1");
        set.recompose();
        assert!(set.resolve("w1").is_none());
        assert!(set.resolve("w0").is_some());
    }

    #[test]
    fn preset_port_override_survives_recompose() {
        let (set, sim) = ws(&[], 5060);
        set.preset("w0", "10.0.0.1".to_string(), 5070, WorkerHealth::Alive, None, Some(7));
        sim.add(Peer::new("w0", "10.0.0.1"));
        set.recompose();
        let w0 = set.resolve("w0").unwrap();
        assert_eq!(w0.address, ProxyAddr::new("10.0.0.1", 5070), "per-worker port_override honoured");
        assert_eq!(w0.health, WorkerHealth::Alive, "preset health honoured");
        assert_eq!(w0.first_seen_at_ms, Some(7));
    }
}
