//! The **views ledger** — who believed what about whom, across the cluster.
//!
//! A failover is a disagreement before it is a failure: the front proxy drops an
//! ordinal the orchestrator withdrew while that worker's process is still bound
//! and serving; a survivor parks a peer the orchestrator has already replaced.
//! The message timeline shows neither — every actor behaves correctly for the
//! world it believes in. This ledger records those worlds so the report can put
//! them side by side.
//!
//! ## Two writers, one ledger
//! 1. **The primitives** ([`FailoverHarness::withdraw`](crate::FailoverHarness::withdraw),
//!    `readmit`, `spawn_replacement`, `crash`, drain …) record what they did at
//!    the instant they act; the `signal` is the primitive's own name.
//! 2. **The sampler** ([`ViewLedger::sample`]) reads the LIVE components after
//!    every advance chunk and records only what moved; the `signal` names what
//!    was read (the proxy registry, a supervisor's peer links, a core's drain
//!    latch).
//!
//! So a primitive's entry is an intent and the sampler's is the component's
//! answer — the gap between the two is exactly what a reader wants to see.
//!
//! ## Sampling granularity
//! [`ViewLedger::sample`] runs once per 100 ms advance chunk (the harness's
//! [`advance`](crate::FailoverHarness::advance) drives it through
//! `sip_clock::testkit::pump_sampled`). A belief that flips and flips back
//! inside one chunk is therefore invisible; every belief that outlives a chunk
//! boundary is recorded exactly once, at the first chunk that observed it. The
//! sampler only reads (no `sync`, no latch evaluation, nothing spawned), so it
//! cannot perturb what it observes.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use b2bua::repl::{PeerLink, Readiness, ReplicationSupervisor};
use layer_harness::EventSequencer;
use sip_clock::Clock;
use sip_proxy::registry::simulated::SimulatedWorkerRegistry;
use sip_proxy::registry::{WorkerHealth, WorkerRegistry};
use topology::SimulatedMembership;

/// What one observer holds true about one subject. The variants are the four
/// vocabularies in play — an orchestrator's intent, the proxy registry's
/// annotation, a supervisor's peer link, a process's own state — each rendering
/// its own [`label`](Belief::label) but classified onto one comparable
/// [`stance`](Belief::stance), so different words for the same world do not read
/// as conflict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Belief {
    /// Not in this observer's view at all.
    Absent,
    /// The proxy registry holds the subject at this health.
    Registered(WorkerHealth),
    /// A replication peer whose flows are running.
    PeerActive,
    /// A replication peer that left membership: flows interrupted, watermarks
    /// retained.
    PeerParked,
    /// The subject's process is up and serving at this incarnation gen.
    Running { gen: u64 },
    /// The process is up but draining (terminal, still serving live calls).
    Draining { gen: u64 },
    /// The process is gone.
    Dead { gen: u64 },
    /// Orchestrator: the endpoint is withdrawn (the process is NOT touched).
    Withdrawn,
    /// Orchestrator: the endpoint is published again.
    Admitted,
}

impl Belief {
    /// The observer's own words, as the report displays them.
    pub fn label(&self) -> String {
        match self {
            Belief::Absent => "absent".into(),
            Belief::Registered(h) => format!("present/{h:?}"),
            Belief::PeerActive => "peer active".into(),
            Belief::PeerParked => "peer parked".into(),
            Belief::Running { gen } => format!("running gen {gen}"),
            Belief::Draining { gen } => format!("draining gen {gen}"),
            Belief::Dead { gen } => format!("dead gen {gen}"),
            Belief::Withdrawn => "withdrawn".into(),
            Belief::Admitted => "admitted".into(),
        }
    }

    /// The comparable classification two observers are said to agree on.
    pub fn stance(&self) -> &'static str {
        match self {
            Belief::Absent | Belief::PeerParked | Belief::Withdrawn => "absent",
            Belief::Registered(WorkerHealth::Dead) | Belief::Dead { .. } => "dead",
            Belief::Registered(WorkerHealth::Draining) | Belief::Draining { .. } => "draining",
            Belief::Registered(_) | Belief::PeerActive | Belief::Running { .. } => "present",
            Belief::Admitted => "present",
        }
    }
}

/// One recorded belief change: `observer` held `belief` about `subject` from
/// this instant, because of `signal`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct View {
    /// Virtual-clock instant (ms).
    pub at_ms: i64,
    /// Global recording-order sequence, shared with the SIP + repl planes.
    pub seq: u64,
    /// Who holds it (`orchestrator`, `proxy`, or an incarnation key `b1#g2`).
    pub observer: String,
    /// Who it is about (a worker ordinal).
    pub subject: String,
    /// The belief.
    pub belief: Belief,
    /// The primitive that acted, or the component that was read.
    pub signal: String,
}

/// One registered worker incarnation: its identity plus the shared handles the
/// sampler reads. Handles are clone-cheap views of the LIVE components, so an
/// incarnation stays observable after the test moved (or crashed) its SUT.
pub struct Incarnation {
    /// Cluster ordinal (`b1`).
    pub ordinal: String,
    /// Incarnation gen (1 on first spawn, +1 per reboot/replacement).
    pub gen: u64,
    /// This incarnation's SIP wire address.
    pub sip_addr: SocketAddr,
    /// This incarnation's replication listen address.
    pub repl_addr: SocketAddr,
    /// Cleared by the SUT when the process dies (`crash`/`reboot`).
    pub alive: Arc<AtomicBool>,
    /// The node's replication supervisor — its per-peer link beliefs.
    pub supervisor: Option<ReplicationSupervisor>,
    /// The node's readiness handle — its own drain latch.
    pub readiness: Readiness,
    /// The node's membership view, so a cluster primitive can drive a peer
    /// delta into it.
    pub membership: Arc<SimulatedMembership>,
}

impl Incarnation {
    /// The observer key: `b1#g2`.
    pub fn key(&self) -> String {
        format!("{}#g{}", self.ordinal, self.gen)
    }

    fn is_alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }
}

/// One incarnation's report axis: the column it renders on, its addresses, and
/// whether its ordinal ever ran two incarnations at once (which is what makes an
/// ordinal fan out into per-incarnation sub-lanes).
#[derive(Clone, Debug)]
pub struct IncarnationAxis {
    pub ordinal: String,
    pub gen: u64,
    pub sip_addr: SocketAddr,
    pub repl_addr: SocketAddr,
    /// This ordinal had two incarnations alive at once during the run.
    pub fanned: bool,
}

struct Inner {
    views: Vec<View>,
    incarnations: Vec<Incarnation>,
    /// Ordinals that ever had two live incarnations at once.
    fanned: Vec<String>,
    proxy: Option<SimulatedWorkerRegistry>,
    /// `(observer, subject) → last recorded belief` — the change filter.
    last: HashMap<(String, String), Belief>,
}

/// The cluster's observers and every belief change they went through. Shared
/// (`Arc`) between the harness, which samples it, and the worker SUTs, which
/// record their own lifecycle into it.
pub struct ViewLedger {
    inner: Mutex<Inner>,
    clock: Clock,
    seq: Arc<EventSequencer>,
}

impl ViewLedger {
    /// Build an empty ledger on the harness's clock + shared sequencer, so a
    /// belief change interleaves with SIP messages and repl frames in true
    /// append order.
    pub fn new(clock: Clock, seq: Arc<EventSequencer>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Inner {
                views: Vec::new(),
                incarnations: Vec::new(),
                fanned: Vec::new(),
                proxy: None,
                last: HashMap::new(),
            }),
            clock,
            seq,
        })
    }

    /// Attach the proxy's worker registry — the `proxy` observer's source.
    pub fn attach_proxy(&self, registry: SimulatedWorkerRegistry) {
        self.inner.lock().unwrap().proxy = Some(registry);
    }

    /// Register a freshly spawned incarnation and record its first self-belief.
    /// An ordinal that already has a LIVE incarnation is marked fanned: from
    /// here on it renders as per-incarnation sub-lanes.
    pub fn register(&self, incarnation: Incarnation) {
        {
            let mut inner = self.inner.lock().unwrap();
            let ord = incarnation.ordinal.clone();
            if inner.incarnations.iter().any(|i| i.ordinal == ord && i.is_alive())
                && !inner.fanned.contains(&ord)
            {
                inner.fanned.push(ord);
            }
            inner.incarnations.push(incarnation);
        }
        let (key, ordinal, gen) = {
            let inner = self.inner.lock().unwrap();
            let i = inner.incarnations.last().expect("just pushed");
            (i.key(), i.ordinal.clone(), i.gen)
        };
        self.record(&key, &ordinal, Belief::Running { gen }, "spawn");
    }

    /// Record a belief, unconditionally (a primitive stating what it just did).
    pub fn record(&self, observer: &str, subject: &str, belief: Belief, signal: &str) {
        let view = View {
            at_ms: self.clock.now_ms(),
            seq: self.seq.next(),
            observer: observer.to_string(),
            subject: subject.to_string(),
            belief,
            signal: signal.to_string(),
        };
        let mut inner = self.inner.lock().unwrap();
        inner.last.insert((observer.to_string(), subject.to_string()), belief);
        inner.views.push(view);
    }

    /// Record a belief only if it differs from this observer's last one about
    /// this subject (the sampler's filter).
    fn record_change(&self, observer: &str, subject: &str, belief: Belief, signal: &str) {
        let unchanged = self
            .inner
            .lock()
            .unwrap()
            .last
            .get(&(observer.to_string(), subject.to_string()))
            .is_some_and(|b| *b == belief);
        if !unchanged {
            self.record(observer, subject, belief, signal);
        }
    }

    /// Read the LIVE components and record every belief that moved — the
    /// harness runs this after each advance chunk (see the module docs).
    pub fn sample(&self) {
        let subjects: Vec<String> = {
            let inner = self.inner.lock().unwrap();
            let mut ords: Vec<String> = Vec::new();
            for i in &inner.incarnations {
                if !ords.contains(&i.ordinal) {
                    ords.push(i.ordinal.clone());
                }
            }
            ords
        };

        // The proxy's registry projection: membership presence ⊕ health. The
        // handle is cloned OUT of the lock before the `if let` — its scrutinee
        // temporary would otherwise hold the guard across `record_change`.
        let registry = self.inner.lock().unwrap().proxy.clone();
        if let Some(registry) = registry {
            for subject in &subjects {
                let belief = match registry.resolve(subject) {
                    Some(entry) => Belief::Registered(entry.health),
                    None => Belief::Absent,
                };
                self.record_change("proxy", subject, belief, "proxy registry");
            }
        }

        // Each incarnation: its own state, and — while it lives — its view of
        // every peer.
        let snapshot: Vec<(String, String, u64, bool, bool, Option<ReplicationSupervisor>)> = {
            let inner = self.inner.lock().unwrap();
            inner
                .incarnations
                .iter()
                .map(|i| {
                    (
                        i.key(),
                        i.ordinal.clone(),
                        i.gen,
                        i.is_alive(),
                        i.readiness.is_draining(),
                        i.supervisor.clone(),
                    )
                })
                .collect()
        };
        for (key, ordinal, gen, alive, draining, supervisor) in snapshot {
            let own = match (alive, draining) {
                (false, _) => Belief::Dead { gen },
                (true, true) => Belief::Draining { gen },
                (true, false) => Belief::Running { gen },
            };
            self.record_change(&key, &ordinal, own, "core readiness");
            if !alive {
                continue;
            }
            let Some(supervisor) = supervisor else { continue };
            for subject in &subjects {
                if *subject == ordinal {
                    continue;
                }
                let belief = match supervisor.peer_link(subject) {
                    PeerLink::Active => Belief::PeerActive,
                    PeerLink::Parked => Belief::PeerParked,
                    PeerLink::Absent => Belief::Absent,
                };
                self.record_change(&key, subject, belief, "supervisor peers");
            }
        }
    }

    /// Every recorded belief change, in record order.
    pub fn views(&self) -> Vec<View> {
        self.inner.lock().unwrap().views.clone()
    }

    /// The beliefs `observer` went through about `subject`, in order — the shape
    /// a test asserts on.
    pub fn beliefs(&self, observer: &str, subject: &str) -> Vec<Belief> {
        self.inner
            .lock()
            .unwrap()
            .views
            .iter()
            .filter(|v| v.observer == observer && v.subject == subject)
            .map(|v| v.belief)
            .collect()
    }

    /// The report axes, in `(repl addr, gen)` order — one entry per incarnation
    /// of a fanned-out ordinal, one entry (its latest incarnation) otherwise.
    pub fn axes(&self) -> Vec<IncarnationAxis> {
        let inner = self.inner.lock().unwrap();
        let mut axes: Vec<IncarnationAxis> = Vec::new();
        for i in &inner.incarnations {
            let fanned = inner.fanned.contains(&i.ordinal);
            let axis = IncarnationAxis {
                ordinal: i.ordinal.clone(),
                gen: i.gen,
                sip_addr: i.sip_addr,
                repl_addr: i.repl_addr,
                fanned,
            };
            match (fanned, axes.iter().position(|a| a.ordinal == i.ordinal)) {
                // Collapsed ordinal: the latest incarnation owns the column.
                (false, Some(pos)) => axes[pos] = axis,
                _ => axes.push(axis),
            }
        }
        axes.sort_by_key(|a| (a.repl_addr.port(), a.gen));
        axes
    }

    /// The highest incarnation gen ever registered for `ordinal` (`0` if none) —
    /// what a replacement's gen is derived from.
    pub fn max_gen(&self, ordinal: &str) -> u64 {
        self.inner
            .lock()
            .unwrap()
            .incarnations
            .iter()
            .filter(|i| i.ordinal == ordinal)
            .map(|i| i.gen)
            .max()
            .unwrap_or(0)
    }

    /// The live incarnations OF `ordinal`, as `(key, membership)` — the worker's
    /// own view of the pool, which the informer shows it (ADR-0031 D6).
    pub fn live_incarnations_of(&self, ordinal: &str) -> Vec<(String, Arc<SimulatedMembership>)> {
        self.inner
            .lock()
            .unwrap()
            .incarnations
            .iter()
            .filter(|i| i.ordinal == ordinal && i.is_alive())
            .map(|i| (i.key(), i.membership.clone()))
            .collect()
    }

    /// The live incarnations that are NOT of `ordinal`, as `(key, membership)` —
    /// the peers a cluster membership primitive must drive.
    pub fn live_peers_of(&self, ordinal: &str) -> Vec<(String, Arc<SimulatedMembership>)> {
        self.inner
            .lock()
            .unwrap()
            .incarnations
            .iter()
            .filter(|i| i.ordinal != ordinal && i.is_alive())
            .map(|i| (i.key(), i.membership.clone()))
            .collect()
    }

    /// The proxy's registry handle, once attached.
    pub fn proxy_registry(&self) -> Option<SimulatedWorkerRegistry> {
        self.inner.lock().unwrap().proxy.clone()
    }

    /// Every SIP address any incarnation ever bound (the RFC-audit exclusion set
    /// must name them all — a worker bind is never judged as a UA).
    pub fn all_sip_addrs(&self) -> Vec<SocketAddr> {
        self.inner.lock().unwrap().incarnations.iter().map(|i| i.sip_addr).collect()
    }
}
