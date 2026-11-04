//! The proxy-only overlay on the worker set, keyed by ordinal: each worker's
//! health and acceptance as the OPTIONS probe reads them, and its port. Lifted
//! out of the worker set, so that the set is membership and a
//! [`WorkerEntry`](super::WorkerEntry) a pure projection.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use arc_swap::ArcSwap;
use topology::Peer;

use crate::addr::ProxyAddr;
use crate::registry::{Acceptance, WorkerHealth, WorkerId};

/// Per-ordinal proxy annotation — everything a [`WorkerEntry`](super::WorkerEntry)
/// carries that membership does not. `host` is shadowed only to detect an address
/// move (→ a fresh, re-probed endpoint); routing always takes the host from
/// membership at projection time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Annot {
    pub(super) health: WorkerHealth,
    pub(super) acceptance: Acceptance,
    pub(super) host: String,
    /// Per-worker port. `None` → use the registry's `default_port` (the
    /// production/k8s case; every worker shares the cluster SIP port).
    pub(super) port_override: Option<u16>,
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

/// What the overlay stopped serving: an ordinal gone from the ready members, or
/// moved off a host.
pub(super) struct Departed {
    pub(super) id: WorkerId,
    pub(super) address: ProxyAddr,
    /// Its record read `Alive` when it was dropped.
    pub(super) read_alive: bool,
}

/// Proxy-only worker annotations keyed by ordinal — the OPTIONS/health + port
/// concerns lifted out of the worker set. Lock-free reads via [`ArcSwap`].
#[derive(Default)]
pub(super) struct WorkerAnnotations {
    records: ArcSwap<HashMap<WorkerId, Annot>>,
}

impl WorkerAnnotations {
    pub(super) fn snapshot(&self) -> Arc<HashMap<WorkerId, Annot>> {
        self.records.load_full()
    }

    /// Read-modify-write helper (annotations only mutate off the hot path).
    /// `rcu`, not load-clone-store: the probe task's `set_health` and the
    /// reconcile task's `sync_membership` run concurrently, and a plain store
    /// could publish a map cloned BEFORE the other writer's update — silently
    /// erasing a `Dead` mark until the next probe tick re-wrote it (a window
    /// where new dialogs kept routing to a dead worker). `rcu` re-runs the
    /// closure on contention, so `f` must be idempotent over its captures.
    pub(super) fn update(&self, f: impl Fn(&mut HashMap<WorkerId, Annot>)) {
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
    /// Returns what the overlay stopped serving — a departure or a host move —
    /// for the caller to tombstone. The diff is read from the map this write
    /// replaced, so of two recomposes racing over one change only one returns
    /// it, and a retried closure cannot double-log it.
    pub(super) fn sync_membership(&self, peers: &[Peer], default_port: u16) -> Vec<Departed> {
        let live: HashSet<&str> = peers.iter().map(|p| p.ordinal.as_str()).collect();
        let replaced = self.records.rcu(|cur| {
            let mut recs = (**cur).clone();
            recs.retain(|id, _| live.contains(id.as_str()));
            for p in peers {
                match recs.get(&p.ordinal) {
                    Some(rec) if rec.host == p.host => {} // unchanged → preserve
                    _ => {
                        recs.insert(p.ordinal.clone(), Annot::fresh(p.host.clone()));
                    }
                }
            }
            recs
        });
        let departed_of = |id: &WorkerId, rec: &Annot| Departed {
            id: id.clone(),
            address: ProxyAddr::new(rec.host.clone(), rec.port_override.unwrap_or(default_port)),
            read_alive: rec.health == WorkerHealth::Alive,
        };
        // Membership deltas are rare state changes: each gets its own line.
        let mut departed = Vec::new();
        for (id, rec) in replaced.iter() {
            if !live.contains(id.as_str()) {
                tracing::info!(
                    node = observe::node(),
                    worker = %id,
                    host = %rec.host,
                    "worker left the registry"
                );
                departed.push(departed_of(id, rec));
            }
        }
        for p in peers {
            match replaced.get(&p.ordinal) {
                Some(rec) if rec.host == p.host => {}
                Some(rec) => {
                    tracing::info!(
                        node = observe::node(),
                        worker = %p.ordinal,
                        from_host = %rec.host,
                        host = %p.host,
                        "worker re-created at a new host"
                    );
                    departed.push(departed_of(&p.ordinal, rec));
                }
                None => tracing::info!(
                    node = observe::node(),
                    worker = %p.ordinal,
                    host = %p.host,
                    "worker joined the registry"
                ),
            }
        }
        departed
    }

    /// Annotate health and acceptance of new calls in one write, `None`
    /// leaving that one as it is. No-op if the worker is unknown or unchanged.
    /// Returns whether the write turned the worker's health `Dead`.
    ///
    /// A write reading a worker `Alive` while the view reads no worker alive
    /// returns every other worker read `Dead` to `Unknown`: verdicts taken
    /// while the proxy reached no worker speak of its own reach, not of the
    /// workers, so a rejoining proxy routes no response or request away from
    /// a worker on them. Its probe judges each afresh.
    pub(super) fn set_view(
        &self,
        id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<Acceptance>,
    ) -> bool {
        let snapshot = self.snapshot();
        let Some(previous) = snapshot.get(id).map(|r| (r.health, r.acceptance)) else {
            return false;
        };
        let next = (health.unwrap_or(previous.0), acceptance.unwrap_or(previous.1));
        if next == previous {
            return false;
        }
        let blind = !snapshot.values().any(|r| r.health == WorkerHealth::Alive);
        let forgotten: Vec<WorkerId> = if blind && next.0 == WorkerHealth::Alive {
            snapshot
                .iter()
                .filter(|(other, r)| other.as_str() != id && r.health == WorkerHealth::Dead)
                .map(|(other, _)| other.clone())
                .collect()
        } else {
            Vec::new()
        };
        for other in &forgotten {
            tracing::info!(
                node = observe::node(),
                worker = %other,
                read_alive = id,
                "dead verdict taken while no worker read alive forgotten"
            );
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
            for other in &forgotten {
                if let Some(rec) = recs.get_mut(other).filter(|r| r.health == WorkerHealth::Dead) {
                    rec.health = WorkerHealth::Unknown;
                }
            }
        });
        next.0 == WorkerHealth::Dead && previous.0 != WorkerHealth::Dead
    }
}
