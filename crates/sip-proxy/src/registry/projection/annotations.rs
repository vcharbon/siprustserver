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
    /// Its endpoint read `ready` at the last recompose. A turn to `Dead` of a
    /// worker neither ready nor read `Alive` is no verdict: a booting pod, or
    /// a worker whose flip was told already.
    pub(super) ready: bool,
    /// The projection holds it: what a departure tombstones.
    pub(super) routed: bool,
}

impl Annot {
    /// A brand-new annotation for a just-(re)joined endpoint: `Unknown`, not
    /// routable until the OPTIONS probe reads it `Alive` (ADR-0050 §8).
    fn fresh(host: String, standing: Standing) -> Self {
        Self {
            health: WorkerHealth::Unknown,
            acceptance: Acceptance::Accept,
            host,
            port_override: None,
            ready: standing.ready,
            routed: standing.routed,
        }
    }
}

/// A member's standing in the view this recompose reads.
#[derive(Debug, Clone, Copy)]
pub(super) struct Standing {
    pub(super) ready: bool,
    pub(super) routed: bool,
}

/// The ordinals the overlay keeps a record for, with their standing: the
/// members the probe judges (ADR-0031 D1). `no_ready` names a view where no
/// member reads ready, whose members are all routed on the probe's answers.
pub(super) fn standing_of(peer: &Peer, no_ready: bool) -> Option<Standing> {
    (peer.ready || !peer.terminating)
        .then_some(Standing { ready: peer.ready, routed: peer.ready || no_ready })
}

/// What left the projection: an ordinal that left the members the probe judges,
/// moved off a host, or stopped being routed.
pub(super) struct Departed {
    pub(super) id: WorkerId,
    pub(super) address: ProxyAddr,
    /// A worker read `Alive` whose endpoint flipped not ready with another
    /// member still ready, or moved to a host where it reads not ready.
    pub(super) flipped: bool,
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

    /// Keep the overlay in step with the view: drop every ordinal the probe no
    /// longer judges, seed a fresh record for a new one, and **reset** one whose
    /// host moved (a recreated pod must be re-probed — a stale `Dead`/`Alive` must
    /// not survive). An unchanged ordinal keeps its probe-written health and
    /// port, unless it is a flip of a worker read `Alive`: the flip is its verdict
    /// and its health `Unknown` until it answers again, so one death tells once.
    /// This is the only proxy-specific lifecycle rule; it has no membership twin.
    ///
    /// Returns what the projection stopped serving for the caller to tombstone.
    /// The diff is read from the map this write replaced, so of two recomposes
    /// racing over one change only one returns it, and a retried closure cannot
    /// double-log it.
    pub(super) fn sync_membership(
        &self,
        members: &[Peer],
        no_ready: bool,
        default_port: u16,
    ) -> Vec<Departed> {
        let kept: Vec<(&Peer, Standing)> =
            members.iter().filter_map(|p| Some((p, standing_of(p, no_ready)?))).collect();
        let live: HashSet<&str> = kept.iter().map(|(p, _)| p.ordinal.as_str()).collect();
        let replaced = self.records.rcu(|cur| {
            let mut recs = (**cur).clone();
            recs.retain(|id, _| live.contains(id.as_str()));
            for (p, standing) in &kept {
                match recs.get_mut(&p.ordinal) {
                    Some(rec) if rec.host == p.host => {
                        if flips(rec, *standing) {
                            rec.health = WorkerHealth::Unknown;
                        }
                        rec.ready = standing.ready;
                        rec.routed = standing.routed;
                    }
                    _ => {
                        recs.insert(p.ordinal.clone(), Annot::fresh(p.host.clone(), *standing));
                    }
                }
            }
            recs
        });
        let departed_of = |id: &WorkerId, rec: &Annot, flipped: bool| Departed {
            id: id.clone(),
            address: ProxyAddr::new(rec.host.clone(), rec.port_override.unwrap_or(default_port)),
            flipped,
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
                if rec.routed {
                    departed.push(departed_of(id, rec, false));
                }
            }
        }
        for (p, standing) in &kept {
            match replaced.get(&p.ordinal) {
                Some(rec) if rec.host == p.host => {
                    if rec.routed && !standing.routed {
                        tracing::info!(
                            node = observe::node(),
                            worker = %p.ordinal,
                            host = %p.host,
                            "worker reads not ready, another reads ready: out of the routing"
                        );
                        departed.push(departed_of(&p.ordinal, rec, flips(rec, *standing)));
                    }
                }
                Some(rec) => {
                    tracing::info!(
                        node = observe::node(),
                        worker = %p.ordinal,
                        from_host = %rec.host,
                        host = %p.host,
                        "worker re-created at a new host"
                    );
                    if rec.routed {
                        departed.push(departed_of(&p.ordinal, rec, flips(rec, *standing)));
                    }
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
    /// Returns whether the write is a dead verdict: it turned the worker's
    /// health `Dead` from `Alive`, or from `Unknown` on an endpoint that reads
    /// ready. A worker not ready and never read `Alive` since its flip's verdict
    /// or its join stands on no answer that could turn out wrong.
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
        let Some(record) = snapshot.get(id) else {
            return false;
        };
        let previous = (record.health, record.acceptance);
        let was_ready = record.ready;
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
        next.0 == WorkerHealth::Dead
            && match previous.0 {
                WorkerHealth::Alive => true,
                WorkerHealth::Unknown => was_ready,
                WorkerHealth::Dead => false,
            }
    }
}

/// Whether a worker this record describes flips with `standing`: read `Alive`,
/// its endpoint ready until now and not ready any more, and out of the routing
/// for it, a ready member remaining.
fn flips(rec: &Annot, standing: Standing) -> bool {
    rec.health == WorkerHealth::Alive && rec.ready && !standing.ready && !standing.routed
}
