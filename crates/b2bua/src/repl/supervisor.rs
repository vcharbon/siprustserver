//! [`ReplicationSupervisor`] — ties per-`(peer, flow)` [`Puller`]s to cluster
//! topology (ADR-0014 §Stream topology).
//!
//! Each peer is replicated over **two single-flow sockets**: a **Reclaim** flow
//! (`partition = Pri` — the peer's calls we hold as backup, that we reclaim on
//! reboot) and a **Backup** flow (`partition = Bak` — our own calls the peer
//! backs up). The supervisor runs one [`Puller`] per `(ordinal, flow)`.
//!
//! On start it `snapshot`s membership, spawns the **Reclaim** pullers, and — once
//! Reclaim readiness is reached — spawns the **Backup** pullers; it then
//! subscribes to `changes()` and reconciles:
//! - `Added` → spawn/Connecting (or un-Park: reconnect from the retained W).
//! - `Removed` → `Parked`: interrupt both flows, **retain W forever** keyed by
//!   `(ordinal, flow)`, and tell the router the peer departed (ADR-0047).
//! - `AddressChanged` → reconnect both flows to the new binding (host or worker
//!   identity, ADR-0055 §6) from the retained W.
//! - `ConditionsChanged` → nothing moves: a member is **pulled while it is in the
//!   snapshot at all**, ready or not, terminating or not (ADR-0031 D1). Only
//!   leaving the snapshot parks it. A peer pulled while not ready is logged once
//!   and counted on the `b2bua_repl_peers_pulled_not_ready` gauge.
//!
//! Every snapshot is also read for this node's **own** endpoint before self is
//! filtered out of the desired set: once seen routable, its disappearance is the
//! node's withdrawal from routing (ADR-0031 D6), and its ordinal bound to
//! another worker identity is its supersession (ADR-0055 §6) — the
//! [`SelfEndpoint`](super::SelfEndpoint) state, exposed as
//! [`is_withdrawn`](ReplicationSupervisor::is_withdrawn) and on a watch the core
//! turns into the `Draining` latch and its fence. Only a source that shows a
//! member itself ([`Membership::observes_self`]) is read this way.
//!
//! ## Boot order: Reclaim first, Backup deferred
//! A rebooting node prioritises reclaiming **its own** partition over taking on
//! backup duty for others. Reclaim streams open immediately; the Backup streams
//! open only after the Reclaim flows are all current-or-unreachable (the
//! readiness gate). Backup never feeds readiness — its `current` flag is
//! metrics-only — so deferring it is purely a bandwidth-prioritisation choice
//! and has no correctness role (`(p,b)` makes any incidental overlap safe).
//!
//! ## Watermark retention per `(ordinal, flow)`
//! The authoritative watermark + current flag live in the supervisor, keyed by
//! `(ordinal, flow)`, and **survive** Park/disconnect/re-add. A running puller
//! publishes its progress on a `watch` channel the supervisor mirrors; a
//! re-spawned puller is seeded from the retained W so it resumes.
//!
//! ## Introspection (for tests / readiness) — Reclaim-scoped
//! [`is_current`](ReplicationSupervisor::is_current) /
//! [`all_current`](ReplicationSupervisor::all_current) /
//! [`bootstrap_complete`](ReplicationSupervisor::bootstrap_complete) /
//! [`watermark`](ReplicationSupervisor::watermark) all read the **Reclaim** flow
//! — readiness depends on reclaiming our own partition, never on backing up
//! others. The readiness state machine consumes `all_current`/`all_bootstrapped`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use repl_net::frame::{Partition, Watermark};
use repl_net::transport::ReplicationNetwork;
use tokio::sync::watch;
use topology::{Membership, Peer};

use super::peer_resolver::AddrResolver;
use super::puller::{Puller, PullerConfig, PullerStatus};
use super::self_endpoint::SelfObserver;
use super::ReplicatingCallStore;
use crate::poison::{locked, SUPERVISOR_PEERS, SUPERVISOR_WIRING};

mod backup_deferral;
mod follow;
mod own_endpoint;
mod wiring;

/// One of the supervisor's wiring slots ([`SUPERVISOR_WIRING`]).
fn wiring<T>(slot: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    locked(slot, SUPERVISOR_WIRING)
}

/// Default cadence of the supervisor's belt-and-suspenders snapshot reconcile
/// (ADR-0012 D2): re-reads membership every 5 s and acts only on drift, so any
/// missed/lagged delta self-heals. Cheap (a handful of peers); a no-op when the
/// set is unchanged.
const DEFAULT_RECONCILE_PERIOD: Duration = Duration::from_secs(5);

/// Poll cadence of the backup-deferral gate: how often it re-checks whether the
/// Reclaim flows are ready (and the Backup streams may open). Short so a node
/// starts backing up its peers promptly after re-hydrating its own partition.
const BACKUP_GATE_POLL: Duration = Duration::from_millis(100);

/// Per-`(ordinal, flow)` retained replication state — survives Park/disconnect/
/// re-add. One of these per flow lives inside a [`PeerEntry`].
struct FlowState {
    /// The running puller's status receiver (current flag + live watermark), or
    /// `None` while Parked.
    status_rx: Option<watch::Receiver<PullerStatus>>,
    /// Cancel handle to interrupt the running puller (Park / re-spawn).
    cancel_tx: Option<watch::Sender<bool>>,
    /// The retained watermark — authoritative, survives the puller task. Seeded
    /// into the next puller on re-spawn.
    watermark: Watermark,
    /// Sticky current flag — retained across Park (never cleared, ADR-0011 X6).
    current: bool,
    /// Sticky bootstrap-complete flag (X5) — retained across Park. Set when the
    /// puller hit the first catch-up `Noop`, the hard timer fired, or it resumed
    /// warm.
    bootstrap_complete: bool,
    /// Highest reset generation absorbed from any puller for this flow. A puller
    /// bumps its `reset_gen` when the server pushes `ResetToBootstrap` (watermark
    /// forced back to `(0,0)`); a higher value here means we must pull the
    /// retained watermark DOWN so a respawn re-bootstraps instead of resuming the
    /// now-invalid high W.
    reset_gen: u64,
    /// Sticky: this flow was reached by a successful `connect` at least once. An
    /// unreachable peer (never connected) that goes bootstrap-complete only by the
    /// hard timer must NOT pin readiness NotReady (ADR-0011 X5 — liveness).
    ever_connected: bool,
}

/// Why a reconcile (re)starts a flow's puller.
fn flow_reason(drift: bool, ended: bool) -> &'static str {
    match (drift, ended) {
        (true, _) => "binding moved",
        (false, true) => "puller ended",
        (false, false) => "new peer",
    }
}

impl FlowState {
    /// Whether the flow's puller has ended while it was neither parked nor
    /// replaced: a puller runs until it is cancelled, so only a panic ends
    /// one the supervisor still holds the status of.
    fn puller_ended(&self) -> bool {
        self.status_rx.as_ref().is_some_and(|rx| rx.has_changed().is_err())
    }

    fn cold() -> Self {
        Self {
            status_rx: None,
            cancel_tx: None,
            watermark: Watermark::new(0, 0),
            current: false,
            bootstrap_complete: false,
            reset_gen: 0,
            ever_connected: false,
        }
    }

    /// Fold the puller's latest published status into the retained copy. The
    /// current and ever-connected flags are sticky (OR). A `reset_gen` advance means the server
    /// forced a re-bootstrap: pull the watermark DOWN to `(0,0)` and clear
    /// bootstrap-complete so a respawn re-bootstraps. Otherwise bootstrap-complete
    /// is sticky and the watermark only advances.
    fn absorb(&mut self) {
        if let Some(rx) = &self.status_rx {
            let s = *rx.borrow();
            self.current |= s.current;
            self.ever_connected |= s.ever_connected;
            if s.reset_gen > self.reset_gen {
                self.reset_gen = s.reset_gen;
                self.watermark = s.watermark; // (0,0)
                self.bootstrap_complete = s.bootstrap_complete; // false
            } else {
                self.bootstrap_complete |= s.bootstrap_complete;
                if s.watermark > self.watermark {
                    self.watermark = s.watermark;
                }
            }
        }
    }

    /// True iff this flow is current OR was never reachable and best-effort
    /// completed (the readiness predicate, ADR-0011 X5). A reachable-then-blipped
    /// flow keeps the strict gate (sticky `current`).
    fn ready(&self) -> bool {
        self.current || (self.bootstrap_complete && !self.ever_connected)
    }
}

/// Per-ordinal retained state: one [`FlowState`] per flow + the last binding.
struct PeerEntry {
    /// Reclaim flow (`partition = Pri`) — drives readiness + reclaim.
    reclaim: FlowState,
    /// Backup flow (`partition = Bak`) — metrics-only, deferred until ready.
    backup: FlowState,
    /// The membership binding — `host` and worker identity — the running
    /// pullers were last spawned for. The drift signal for the periodic
    /// snapshot reconcile (ADR-0012 D2): a desired peer whose host moved or
    /// whose identity changed (ADR-0055 §6), or whose flows are not running,
    /// is (re)spawned. `None` until a puller has been spawned for this ordinal.
    binding: Option<(String, Option<String>)>,
    /// The peer's `ready` condition as last reconciled. Not a drift signal — a
    /// condition flip never respawns a flow — only what [`PeerLink::Kept`], the
    /// transition log and the pulled-not-ready gauge read.
    ready: bool,
}

impl PeerEntry {
    fn cold() -> Self {
        Self { reclaim: FlowState::cold(), backup: FlowState::cold(), binding: None, ready: true }
    }

    fn flow(&self, partition: Partition) -> &FlowState {
        match partition {
            Partition::Pri => &self.reclaim,
            Partition::Bak => &self.backup,
        }
    }

    /// Park BOTH flows: fold progress, interrupt, drop the status receivers. The
    /// retained watermarks/current flags survive.
    fn park(&mut self) {
        for flow in [&mut self.reclaim, &mut self.backup] {
            flow.absorb();
            if let Some(tx) = flow.cancel_tx.take() {
                let _ = tx.send(true);
            }
            flow.status_rx = None;
        }
    }
}

/// This node's link state toward one peer, as the supervisor holds it —
/// read-only introspection ([`peer_link`](ReplicationSupervisor::peer_link)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLink {
    /// The ordinal was never in this node's membership view.
    Absent,
    /// The peer left the membership set: both flows are interrupted, the
    /// retained watermarks survive for a warm resume.
    Parked,
    /// The Reclaim flow's puller is running toward a ready peer.
    Active,
    /// The Reclaim flow's puller is running toward a member that is present in
    /// the snapshot but not `ready` — pulled on presence alone (ADR-0031 D1).
    Kept,
}

/// Ties pullers to topology; owns the per-`(ordinal, flow)` retained watermarks.
#[derive(Clone)]
pub struct ReplicationSupervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    self_ordinal: String,
    /// This node's own endpoint as the membership shows it (ADR-0031 D6).
    self_endpoint: SelfObserver,
    network: Arc<dyn ReplicationNetwork>,
    store: ReplicatingCallStore,
    resolve: AddrResolver,
    config: PullerConfig,
    /// Replication observability, handed to each spawned puller.
    metrics: crate::metrics::B2buaMetrics,
    /// Cadence of the periodic snapshot reconcile (ADR-0012 D2).
    reconcile_period: Duration,
    /// Router command sink handed to every spawned puller (ADR-0011 X11 fail-back).
    /// `None` until [`set_repl_sink`](ReplicationSupervisor::set_repl_sink) wires it
    /// (the live `B2buaCore` does, before `start`); the sim/test supervisors leave
    /// it unset so pullers drive the store only.
    repl_tx: Mutex<Option<super::RouterSink>>,
    /// The memory admission gate handed to every spawned puller (ADR-0037).
    /// `None` until [`set_capacity`](ReplicationSupervisor::set_capacity).
    capacity: Mutex<Option<crate::capacity::CapacityGate>>,
    /// The turn-point seam handed to every spawned puller (ADR-0055 §13).
    turn_probe: Mutex<crate::turn_probe::TurnProbes>,
    /// Set once the Reclaim flows are all ready and the Backup streams may open
    /// (ADR-0014 boot order). Latched true; subsequent reconciles spawn Backup
    /// pullers for any newly-added peer.
    backup_enabled: AtomicBool,
    /// The membership view (retained so the backup-deferral gate can re-snapshot
    /// and reconcile when readiness is reached).
    membership: Mutex<Option<Arc<dyn Membership>>>,
    /// `ordinal → PeerEntry`.
    peers: Mutex<HashMap<String, PeerEntry>>,
    /// The topology-reconcile loop's handle, retained so [`shutdown`] can abort
    /// it. Without this the loop outlives `crash()`/`shutdown()` (it holds an
    /// `Arc<SupervisorInner>`) and keeps spawning pullers against a dead node on
    /// later membership deltas — a task/memory leak + double-replication.
    ///
    /// [`shutdown`]: ReplicationSupervisor::shutdown
    reconcile: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// The backup-deferral gate task's handle, retained so [`shutdown`] can abort
    /// it (it would otherwise keep polling against a dead node).
    ///
    /// [`shutdown`]: ReplicationSupervisor::shutdown
    gate: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// When [`start`](ReplicationSupervisor::start) wired the membership: the
    /// bootstrap hard timer that bounds an unsynced view runs from here.
    started_at: Mutex<Option<tokio::time::Instant>>,
}

impl SupervisorInner {
    /// The per-peer flows ([`SUPERVISOR_PEERS`]).
    fn peers(&self) -> std::sync::MutexGuard<'_, HashMap<String, PeerEntry>> {
        locked(&self.peers, SUPERVISOR_PEERS)
    }
}

impl ReplicationSupervisor {
    /// Build a supervisor for the worker `self_ordinal` whose identity is
    /// `self_identity` (`None`: it states none, and is never superseded). Call
    /// [`start`](Self::start) to spawn the initial pullers + the
    /// topology-reconcile loop.
    pub fn new(
        self_ordinal: impl Into<String>,
        self_identity: Option<String>,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        resolve: AddrResolver,
        config: PullerConfig,
        metrics: crate::metrics::B2buaMetrics,
    ) -> Self {
        Self::build(self_ordinal, self_identity, network, store, resolve, config, metrics)
    }

    /// Build with explicit puller backoff config (tests inject short backoff).
    /// Replication metrics default to a throwaway here; the live runner uses
    /// [`new`](Self::new) to share the worker's real `B2buaMetrics`.
    pub fn with_config(
        self_ordinal: impl Into<String>,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        resolve: AddrResolver,
        config: PullerConfig,
    ) -> Self {
        Self::build(
            self_ordinal,
            None,
            network,
            store,
            resolve,
            config,
            crate::metrics::B2buaMetrics::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        self_ordinal: impl Into<String>,
        self_identity: Option<String>,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        resolve: AddrResolver,
        config: PullerConfig,
        metrics: crate::metrics::B2buaMetrics,
    ) -> Self {
        let self_ordinal = self_ordinal.into();
        Self {
            inner: Arc::new(SupervisorInner {
                self_endpoint: SelfObserver::new(self_ordinal.clone(), self_identity),
                self_ordinal: self_ordinal.into(),
                network,
                store,
                resolve,
                config,
                metrics,
                reconcile_period: DEFAULT_RECONCILE_PERIOD,
                repl_tx: Mutex::new(None),
                capacity: Mutex::new(None),
                turn_probe: Mutex::new(Default::default()),
                backup_enabled: AtomicBool::new(false),
                membership: Mutex::new(None),
                peers: Mutex::new(HashMap::new()),
                reconcile: Mutex::new(None),
                gate: Mutex::new(None),
                started_at: Mutex::new(None),
            }),
        }
    }

    /// Spawn the Reclaim pullers per current peer (excluding self), keep them in
    /// step with membership, and run the backup-deferral gate that opens the
    /// Backup streams once the Reclaim flows are ready.
    ///
    /// Both the boot snapshot and every subsequent wakeup (delta, `Lagged`
    /// overflow, or periodic tick) flow through one idempotent
    /// `reconcile_from_snapshot`, run as the node `start` is called under: the
    /// boot pass synchronously, under a subscription taken before it, then the
    /// shared [`topology::follow_membership`] loop with its 5 s ticker and
    /// `Lagged` handling (ADR-0012 D1/D2). The loop and the gate are restarted
    /// and counted when a run panics ([`crate::supervise`]); a restarted loop
    /// subscribes and reconciles before it follows.
    pub fn start(&self, membership: Arc<dyn Membership>) {
        *wiring(&self.inner.membership) = Some(membership.clone());
        *wiring(&self.inner.started_at) = Some(tokio::time::Instant::now());
        if membership.observes_self() {
            self.inner.self_endpoint.enable();
        }
        let handle = self.spawn_membership_follow(membership.clone());
        *wiring(&self.inner.reconcile) = Some(handle);

        // Backup-deferral gate: open the Backup streams once the Reclaim flows
        // are all current-or-unreachable (ADR-0014 boot order).
        let this = self.clone();
        let gate = crate::supervise::spawn(crate::supervise::BACKUP_GATE, move || {
            let (this, membership) = (this.clone(), membership.clone());
            async move { this.run_backup_gate(membership).await }
        });
        *wiring(&self.inner.gate) = Some(gate);
    }

    /// Reconcile the running pullers to a fresh membership `snapshot` (ADR-0012
    /// D1/D2). Acts only on drift: a desired peer whose Reclaim flow is **not
    /// running** or whose **host moved** is (re)spawned; once `backup_enabled` is
    /// latched the Backup flow is spawned/maintained the same way. A running
    /// puller whose ordinal is no longer desired is parked (both flows). An
    /// unchanged set produces no work. Every member of the snapshot is desired,
    /// whatever its `ready` / `terminating` conditions (ADR-0031 D1): a condition
    /// flip is not drift, and only leaving the snapshot parks a peer.
    ///
    /// The whole reconcile holds the `peers` lock — including the `tokio::spawn`
    /// of each puller (no await points) — so a concurrent caller (the periodic
    /// loop vs. the backup gate) cannot interleave check-then-spawn and
    /// double-spawn a flow.
    fn reconcile_from_snapshot(&self, snapshot: Vec<Peer>) {
        self.observe_self(&snapshot);
        let desired: Vec<Peer> =
            snapshot.into_iter().filter(|p| p.ordinal != self.inner.self_ordinal).collect();
        let backup_enabled = self.inner.backup_enabled.load(Ordering::SeqCst);

        let mut peers = self.inner.peers();

        for peer in &desired {
            let entry = peers.entry(peer.ordinal.clone()).or_insert_with(PeerEntry::cold);
            let binding = (peer.host.clone(), peer.identity.clone());
            let drift = entry.binding.as_ref() != Some(&binding);
            entry.binding = Some(binding);
            if entry.ready != peer.ready {
                entry.ready = peer.ready;
                if peer.ready {
                    tracing::info!(
                        node = observe::node(),
                        peer = %peer.ordinal,
                        host = %peer.host,
                        "replication peer ready again"
                    );
                } else {
                    tracing::info!(
                        node = observe::node(),
                        peer = %peer.ordinal,
                        host = %peer.host,
                        terminating = peer.terminating,
                        "replication peer kept on presence alone (not ready)"
                    );
                }
            }

            // Reclaim always; Backup only once the gate has latched. A puller
            // that ended unparked panicked: it is respawned and counted.
            let (reclaim_ended, backup_ended) =
                (entry.reclaim.puller_ended(), entry.backup.puller_ended());
            if entry.reclaim.status_rx.is_none() || drift || reclaim_ended {
                tracing::info!(
                    node = observe::node(),
                    peer = %peer.ordinal,
                    host = %peer.host,
                    flow = "recovery",
                    reason = flow_reason(drift, reclaim_ended),
                    "replication peer flow started"
                );
                if reclaim_ended {
                    crate::supervise::RESTARTS.restarted(crate::supervise::REPL_PULLER);
                }
                self.spawn_flow(&mut entry.reclaim, peer, Partition::Pri);
            }
            if backup_enabled && (entry.backup.status_rx.is_none() || drift || backup_ended) {
                tracing::info!(
                    node = observe::node(),
                    peer = %peer.ordinal,
                    host = %peer.host,
                    flow = "backup",
                    reason = flow_reason(drift, backup_ended),
                    "replication peer flow started"
                );
                if backup_ended {
                    crate::supervise::RESTARTS.restarted(crate::supervise::REPL_PULLER);
                }
                self.spawn_flow(&mut entry.backup, peer, Partition::Bak);
            }
        }

        // Park: any peer whose ordinal departed the desired set (both flows).
        // A peer still running is a real stop: the router is told once, so it
        // takes over the peer's calls with an INVITE in flight (ADR-0047); a
        // quiescent call stays reactive (ADR-0014 §1).
        let desired_ords: std::collections::HashSet<&str> =
            desired.iter().map(|p| p.ordinal.as_str()).collect();
        for (ord, entry) in peers.iter_mut() {
            if !desired_ords.contains(ord.as_str()) {
                if entry.reclaim.cancel_tx.is_some() || entry.backup.cancel_tx.is_some() {
                    tracing::info!(
                        node = observe::node(),
                        peer = %ord,
                        "replication peer parked (left the membership set)"
                    );
                    if let Some(sink) = wiring(&self.inner.repl_tx).as_ref() {
                        sink.send(crate::router::ReplCommand::PeerDeparted(ord.clone()));
                    }
                }
                entry.park();
            }
        }

        let pulled_not_ready =
            desired.iter().filter(|p| !p.ready).count().try_into().unwrap_or(u64::MAX);
        self.inner.metrics.set_repl_peers_pulled_not_ready(pulled_not_ready);
    }

    /// (Re)spawn one flow's puller into `flow`, seeded from its retained W. Cancels
    /// any running puller first (its W is absorbed). Called only with the `peers`
    /// lock held — `Puller::new` + `tokio::spawn` have no await points.
    fn spawn_flow(&self, flow: &mut FlowState, peer: &Peer, partition: Partition) {
        // Fold the outgoing puller's progress + cancel it.
        flow.absorb();
        if let Some(tx) = flow.cancel_tx.take() {
            let _ = tx.send(true);
        }
        let start_w = flow.watermark;

        // The puller resolves its address FRESH per connect attempt (ADR-0012 D3)
        // via the shared resolver, so a restarted peer's new IP is picked up on
        // reconnect without a membership delta.
        let (puller, status_rx) = Puller::new(
            peer.clone(),
            self.inner.self_ordinal.clone(),
            partition,
            self.inner.resolve.clone(),
            self.inner.network.clone(),
            self.inner.store.clone(),
            self.inner.config,
            start_w,
            self.inner.metrics.clone(),
        );
        // Forward X11 fail-back commands to the router when a live core wired a sink.
        let puller = match wiring(&self.inner.repl_tx).clone() {
            Some(sink) => puller.with_repl_sink(sink),
            None => puller,
        };
        let puller = match wiring(&self.inner.capacity).clone() {
            Some(gate) => puller.with_capacity(gate),
            None => puller,
        };
        let puller = puller.with_turn_probe(wiring(&self.inner.turn_probe).clone());
        let (cancel_tx, cancel_rx) = watch::channel(false);
        flow.status_rx = Some(status_rx);
        flow.cancel_tx = Some(cancel_tx);

        observe::spawn_keeping_node(async move {
            puller.run(cancel_rx).await;
        });
    }

    /// Shut every running puller down (Park them all) + abort the reconcile loop
    /// and the backup gate. The retained watermarks/current flags survive. Used by
    /// [`B2buaCore::abort`](crate::B2buaCore::abort) to stop a crashed worker's
    /// pullers without changing replication behaviour.
    pub fn shutdown(&self) {
        // Stop reacting to membership deltas + the backup gate FIRST so neither can
        // spawn a new puller against this (about-to-be-dead) node after we park the
        // existing ones. Aborting also frees each task's `Arc<SupervisorInner>`.
        if let Some(h) = wiring(&self.inner.reconcile).take() {
            h.abort();
        }
        if let Some(h) = wiring(&self.inner.gate).take() {
            h.abort();
        }
        let mut peers = self.inner.peers();
        for entry in peers.values_mut() {
            entry.park();
        }
    }

    /// Fold every running puller's published status into the retained map. Tests
    /// call this (or any retained read) after advancing the clock.
    fn sync(&self) {
        fold(&mut self.inner.peers());
    }

    /// Is `peer`'s **Reclaim** flow current (its sticky current flag set)? Folds
    /// live status first.
    pub fn is_current(&self, peer: &str) -> bool {
        self.sync();
        self.inner.peers().get(peer).map(|e| e.reclaim.current).unwrap_or(false)
    }

    /// The peer ordinals whose Reclaim flow gates this node's readiness: the
    /// **ready** members of the membership snapshot, minus self. `None` until
    /// membership is wired by [`start`](Self::start) — pre-start no pullers are
    /// spawned, so the retained map is empty and the gates' `None`-means-no-filter
    /// is harmless.
    ///
    /// The readiness gates ([`all_current`](Self::all_current) /
    /// [`all_bootstrapped`](Self::all_bootstrapped)) filter the retained `peers`
    /// map through this, exactly as the backup-deferral gate treats its
    /// `desired` (`run_backup_gate`):
    /// - a peer that has **left** membership is parked and retained (its
    ///   watermark survives for a warm resume, X5) but cannot pin readiness — a
    ///   cold double-restart that sees a peer once at a stale IP and then loses
    ///   it would otherwise wedge NotReady with no puller left to fire the
    ///   bootstrap hard timer (ADR-0012 D2);
    /// - a member that is present but **not ready** is still pulled (ADR-0031
    ///   D1) but cannot pin readiness either: the orchestrator states it is not
    ///   serving, and a terminating pod can linger in the slice indefinitely, so
    ///   a Reclaim flow that reached it and then lost it (`ever_connected`,
    ///   `current=false`) must not hold a rebooting node NotReady for that long.
    fn desired_ordinals(&self) -> Option<std::collections::HashSet<String>> {
        wiring(&self.inner.membership).as_ref().map(|m| {
            m.snapshot()
                .into_iter()
                .filter(|p| p.ready && p.ordinal != self.inner.self_ordinal)
                .map(|p| p.ordinal)
                .collect()
        })
    }

    /// Are ALL **desired** peers' **Reclaim** flows current — or unreachable? (The
    /// readiness gate.) Empty set → `true`. Peers that have left membership or
    /// are present but not ready are excluded
    /// (`desired_ordinals`) so neither a parked entry
    /// nor a pulled-not-ready one can pin readiness. A peer that is bootstrap-
    /// complete only via the hard timer and was **never reached** does NOT block
    /// readiness (per ADR-0011 X5 a node must boot and serve even when peers are
    /// unreachable). A reachable-then-blipped peer **still desired** keeps the
    /// strict gate (sticky `current`).
    pub fn all_current(&self) -> bool {
        self.sync();
        let desired = self.desired_ordinals();
        self.inner
            .peers()
            .iter()
            .filter(|(ord, _)| desired.as_ref().is_none_or(|d| d.contains(ord.as_str())))
            .all(|(_, e)| e.reclaim.ready())
    }

    /// Has `peer`'s **Reclaim** bootstrap completed (first `Noop`, hard timer, or
    /// warm resume)? Folds live status first. Sticky across Park (X5).
    pub fn bootstrap_complete(&self, peer: &str) -> bool {
        self.sync();
        self.inner.peers().get(peer).map(|e| e.reclaim.bootstrap_complete).unwrap_or(false)
    }

    /// Whether the membership source's view is **authoritative** yet
    /// ([`Membership::synced`]). `false` before [`start`](Self::start) wires a
    /// membership at all. The readiness gates' empty-set vacuous truth is only
    /// sound over a synced view: an informer-backed membership starts EMPTY and
    /// fills async, and treating that placeholder as "peerless cluster" let a
    /// rebooting node latch Ready before a single reclaim puller existed —
    /// routing in-dialog traffic at it pre-reclaim (orphan 481s).
    pub fn membership_synced(&self) -> bool {
        wiring(&self.inner.membership).as_ref().is_some_and(|m| m.synced())
    }

    /// Whether readiness may read the membership view: it is
    /// [synced](Self::membership_synced), or the bootstrap hard timer has run
    /// out since [`start`](Self::start) (ADR-0045). Past it the node goes Ready
    /// with the view it has and peers join as the view fills: the risk an
    /// unreachable peer already carries under the same timer.
    pub fn membership_settled(&self) -> bool {
        let bound = Duration::from_millis(self.inner.config.bootstrap_hard_timeout_ms);
        self.membership_synced()
            || wiring(&self.inner.started_at).is_some_and(|at| at.elapsed() >= bound)
    }

    /// Are ALL **desired** peers' **Reclaim** flows bootstrap-complete? (The
    /// readiness gate.) Empty set → `true`. Peers that have left membership or
    /// are present but not ready are excluded
    /// (`desired_ordinals`) so neither a parked entry
    /// nor a pulled-not-ready one can pin readiness.
    pub fn all_bootstrapped(&self) -> bool {
        self.sync();
        let desired = self.desired_ordinals();
        self.inner
            .peers()
            .iter()
            .filter(|(ord, _)| desired.as_ref().is_none_or(|d| d.contains(ord.as_str())))
            .all(|(_, e)| e.reclaim.bootstrap_complete)
    }

    /// The retained **Reclaim** watermark for `peer` (test introspection).
    pub fn watermark(&self, peer: &str) -> Watermark {
        self.flow_watermark(peer, Partition::Pri)
    }

    /// The retained watermark for a specific `(peer, flow)` (test introspection
    /// into the Backup flow, which the readiness accessors do not expose).
    pub fn flow_watermark(&self, peer: &str, partition: Partition) -> Watermark {
        self.sync();
        self.inner
            .peers()
            .get(peer)
            .map(|e| e.flow(partition).watermark)
            .unwrap_or_else(|| Watermark::new(0, 0))
    }

    /// This node's link state toward `peer` ([`PeerLink`]): `Active` while its
    /// Reclaim puller runs toward a ready member, `Kept` while it runs toward a
    /// member that is present but not ready, `Parked` once it left the
    /// membership set (the entry and its watermarks are retained), `Absent` when
    /// the ordinal was never seen. Pure read — it folds no puller status and
    /// spawns nothing.
    pub fn peer_link(&self, peer: &str) -> PeerLink {
        match self.inner.peers().get(peer) {
            None => PeerLink::Absent,
            Some(e) if e.reclaim.status_rx.is_some() && e.ready => PeerLink::Active,
            Some(e) if e.reclaim.status_rx.is_some() => PeerLink::Kept,
            Some(_) => PeerLink::Parked,
        }
    }

    /// The metrics handle this supervisor and its pullers write.
    pub fn metrics(&self) -> &crate::metrics::B2buaMetrics {
        &self.inner.metrics
    }

    /// Whether the **Reclaim** puller is currently running (not Parked) for `peer`.
    pub fn is_running(&self, peer: &str) -> bool {
        self.inner.peers().get(peer).map(|e| e.reclaim.status_rx.is_some()).unwrap_or(false)
    }

    /// Await `peer`'s **Reclaim** flow becoming current (sticky), folding live
    /// status as it ticks. Returns once current; the caller drives the clock.
    pub async fn await_current(&self, peer: &str) {
        loop {
            // Marked seen before the read, so the wait wakes on the next status.
            let rx = self.inner.peers().get(peer).and_then(|e| seen(&e.reclaim));
            if self.is_current(peer) {
                return;
            }
            match rx {
                Some(mut rx) => {
                    if rx.changed().await.is_err() {
                        return;
                    }
                }
                None => return,
            }
        }
    }
}

/// Fold every running puller's published status into `peers`.
fn fold(peers: &mut HashMap<String, PeerEntry>) {
    for entry in peers.values_mut() {
        entry.reclaim.absorb();
        entry.backup.absorb();
    }
}

/// A copy of `flow`'s status receiver with its current status marked seen, so
/// its `changed()` waits for the next publication. `None` while Parked.
fn seen(flow: &FlowState) -> Option<watch::Receiver<PullerStatus>> {
    flow.status_rx.clone().map(|mut rx| {
        rx.mark_unchanged();
        rx
    })
}
