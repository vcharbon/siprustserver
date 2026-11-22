//! The proxy-only overlay on the worker set, keyed by ordinal: each worker's
//! health and acceptance as the OPTIONS probe reads them, and its port. The
//! worker set holds membership alone, and a [`WorkerEntry`](super::WorkerEntry)
//! is the projection of both.

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
    /// The registry clock's millisecond up to which the worker's last late
    /// probe answer is fresh; `None` with no late answer.
    pub(super) slow_until_ms: Option<u64>,
    /// Its endpoint reads not ready and another member ready, but it flipped
    /// while a late answer was fresh: routed as `Slow` while it reads so
    /// (ADR-0069).
    pub(super) held: bool,
}

impl Annot {
    /// A brand-new annotation for a just-(re)joined endpoint: `Unknown`, not
    /// routable until the OPTIONS probe reads it `Alive` (ADR-0050 §8).
    pub(super) fn fresh(host: String, standing: Standing) -> Self {
        Self {
            health: WorkerHealth::Unknown,
            acceptance: Acceptance::Accept,
            host,
            port_override: None,
            ready: standing.ready,
            routed: standing.routed,
            slow_until_ms: None,
            held: false,
        }
    }

    fn serves_dialogs(&self) -> bool {
        matches!(self.health, WorkerHealth::Alive | WorkerHealth::Slow)
    }
}

/// How a health write turned a worker, for the readers of the registry's
/// verdicts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Turn {
    /// No turn a reader is told of.
    None,
    /// A dead verdict.
    Dead,
    /// `Slow` to `Alive`.
    Recovered,
}

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

#[derive(Default)]
pub(super) struct WorkerAnnotations {
    records: ArcSwap<HashMap<WorkerId, Annot>>,
}

impl WorkerAnnotations {
    pub(super) fn snapshot(&self) -> Arc<HashMap<WorkerId, Annot>> {
        self.records.load_full()
    }

    /// Read-modify-write of the records, off the hot path. The probe task's
    /// `set_health` and the reconcile task's `sync_membership` write
    /// concurrently, so the write is an `rcu`: a load-clone-store could publish
    /// a map cloned before the other writer's update and erase it. `rcu` re-runs
    /// `f` on contention, so `f` must be idempotent over its captures.
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
    /// port, unless it is a flip of a worker read `Alive` or `Slow`: with a late
    /// answer fresh at `now_ms` it reads `Slow` and stays routed while it does;
    /// else the flip is its verdict and its health `Unknown` until it answers
    /// again, so one death tells once. This is the only proxy-specific lifecycle
    /// rule; it has no membership twin.
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
        now_ms: u64,
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
                        let held = held_after(rec, *standing, now_ms);
                        if flips(rec, *standing) {
                            rec.health =
                                if held { WorkerHealth::Slow } else { WorkerHealth::Unknown };
                        }
                        rec.held = held;
                        rec.ready = standing.ready;
                        rec.routed = standing.routed || held;
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
                    let held = held_after(rec, *standing, now_ms);
                    if held && !rec.held {
                        tracing::info!(
                            node = observe::node(),
                            worker = %p.ordinal,
                            host = %p.host,
                            "worker reads not ready while a late answer of its is fresh: slow"
                        );
                    }
                    if rec.routed && !standing.routed && !held {
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
    /// A worker held in the routing while its endpoint reads not ready reads
    /// `Slow` where the write says `Alive`. A worker `Unknown` on an endpoint
    /// that reads not ready reads `Dead` where the write says `Slow`: a pod
    /// still booting serves no dialog.
    ///
    /// Returns [`Turn::Dead`] for a dead verdict: the write turned the worker's
    /// health `Dead` from `Alive`, or from `Unknown` or `Slow` on an endpoint
    /// that reads ready or a worker held. A worker not ready and never read
    /// `Alive` since its flip's verdict or its join stands on no answer that
    /// could turn out wrong. Returns [`Turn::Recovered`] for a turn from `Slow`
    /// to `Alive`.
    ///
    /// A write reading a worker `Alive` while the view reads no worker alive or
    /// slow returns every other worker read `Dead` to `Unknown`: verdicts taken
    /// while the proxy reached no worker speak of its own reach, not of the
    /// workers, so a rejoining proxy routes no response or request away from
    /// a worker on them. Its probe judges each afresh.
    pub(super) fn set_view(
        &self,
        id: &str,
        health: Option<WorkerHealth>,
        acceptance: Option<Acceptance>,
    ) -> Turn {
        let snapshot = self.snapshot();
        let Some(record) = snapshot.get(id) else {
            return Turn::None;
        };
        let previous = (record.health, record.acceptance);
        let standing = record.ready || record.held;
        let written = health.map(|h| match (record.health, h) {
            (_, WorkerHealth::Alive) if record.held => WorkerHealth::Slow,
            (WorkerHealth::Unknown, WorkerHealth::Slow) if !standing => WorkerHealth::Dead,
            _ => h,
        });
        let next = (written.unwrap_or(previous.0), acceptance.unwrap_or(previous.1));
        if next == previous {
            return Turn::None;
        }
        let blind = !snapshot.values().any(Annot::serves_dialogs);
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
        match (previous.0, next.0) {
            (WorkerHealth::Alive, WorkerHealth::Dead) => Turn::Dead,
            (WorkerHealth::Unknown | WorkerHealth::Slow, WorkerHealth::Dead) if standing => {
                Turn::Dead
            }
            (WorkerHealth::Slow, WorkerHealth::Alive) => Turn::Recovered,
            _ => Turn::None,
        }
    }

    /// Stamp the worker's last late probe answer fresh up to `until_ms`.
    /// No-op if the worker is unknown.
    pub(super) fn note_late_answer(&self, id: &str, until_ms: u64) {
        self.update(|recs| {
            if let Some(rec) = recs.get_mut(id) {
                rec.slow_until_ms = Some(rec.slow_until_ms.map_or(until_ms, |u| u.max(until_ms)));
            }
        });
    }
}

/// Whether a worker this record describes flips with `standing`: read `Alive`
/// or `Slow`, its endpoint ready until now and not ready any more, and out of
/// the routing for it, a ready member remaining.
fn flips(rec: &Annot, standing: Standing) -> bool {
    rec.serves_dialogs() && rec.ready && !standing.ready && !standing.routed
}

/// Whether the worker this record describes stays in the routing as `Slow`
/// with `standing`, out of it otherwise: its flip comes while a late answer is
/// fresh at `now_ms`, or it was held and still reads `Slow`. A worker whose
/// endpoint reads ready, or that the view routes anyway, is never held.
fn held_after(rec: &Annot, standing: Standing, now_ms: u64) -> bool {
    if standing.ready || standing.routed {
        return false;
    }
    if rec.held {
        return rec.health == WorkerHealth::Slow;
    }
    flips(rec, standing) && rec.slow_until_ms.is_some_and(|until| now_ms <= until)
}
