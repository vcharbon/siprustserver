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
//!   `(ordinal, flow)`.
//! - `AddressChanged` → reconnect both flows to the new addr from the retained W.
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
//! ## Introspection (for tests / S7 readiness) — Reclaim-scoped
//! [`is_current`](ReplicationSupervisor::is_current) /
//! [`all_current`](ReplicationSupervisor::all_current) /
//! [`bootstrap_complete`](ReplicationSupervisor::bootstrap_complete) /
//! [`watermark`](ReplicationSupervisor::watermark) all read the **Reclaim** flow
//! — readiness depends on reclaiming our own partition, never on backing up
//! others. S7's readiness state machine consumes `all_current`/`all_bootstrapped`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use repl_net::frame::{Partition, Watermark};
use repl_net::transport::ReplicationNetwork;
use sip_clock::Clock;
use tokio::sync::{mpsc, watch};
use topology::{Membership, Peer};

use super::puller::{Puller, PullerConfig, PullerStatus};
use super::ReplicatingCallStore;

/// Default cadence of the supervisor's belt-and-suspenders snapshot reconcile
/// (ADR-0012 D2): re-reads membership every 5 s and acts only on drift, so any
/// missed/lagged delta self-heals. Cheap (a handful of peers); a no-op when the
/// set is unchanged.
const DEFAULT_RECONCILE_PERIOD: Duration = Duration::from_secs(5);

/// Poll cadence of the backup-deferral gate: how often it re-checks whether the
/// Reclaim flows are ready (and the Backup streams may open). Short so a node
/// starts backing up its peers promptly after re-hydrating its own partition.
const BACKUP_GATE_POLL: Duration = Duration::from_millis(100);

/// Resolves a [`Peer`] to its replication [`SocketAddr`], **fresh on every
/// connect attempt** (ADR-0012 D3). Async so a real impl can DNS-resolve a stable
/// per-pod name (`tokio::net::lookup_host`) without blocking the runtime; `None`
/// means "unresolvable right now" — the puller treats it as a failed connect and
/// backs off, then retries (re-resolving). For sim tests / bare-IP hosts this is
/// an instant map lookup or parse.
#[async_trait]
pub trait PeerResolver: Send + Sync {
    /// Resolve `peer` to its replication socket address, or `None` if it cannot
    /// be resolved at this instant.
    async fn resolve(&self, peer: &Peer) -> Option<SocketAddr>;
}

/// Shared handle to a [`PeerResolver`].
pub type AddrResolver = Arc<dyn PeerResolver>;

/// Adapter turning a sync closure (`Fn(&Peer) -> SocketAddr`) into a
/// [`PeerResolver`] — the sim-test / static-map resolver shape. Always resolves
/// (never `None`).
pub struct FnPeerResolver<F>(pub F);

#[async_trait]
impl<F> PeerResolver for FnPeerResolver<F>
where
    F: Fn(&Peer) -> SocketAddr + Send + Sync,
{
    async fn resolve(&self, peer: &Peer) -> Option<SocketAddr> {
        Some((self.0)(peer))
    }
}

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
    /// Sticky current flag — retained across Park (never cleared, Decision 6).
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
    /// hard timer must NOT pin readiness NotReady (Decision 4 — liveness).
    ever_connected: bool,
}

impl FlowState {
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
    /// completed (the readiness predicate, Decision 4). A reachable-then-blipped
    /// flow keeps the strict gate (sticky `current`).
    fn ready(&self) -> bool {
        self.current || (self.bootstrap_complete && !self.ever_connected)
    }
}

/// Per-ordinal retained state: one [`FlowState`] per flow + the last host.
struct PeerEntry {
    /// Reclaim flow (`partition = Pri`) — drives readiness + reclaim.
    reclaim: FlowState,
    /// Backup flow (`partition = Bak`) — metrics-only, deferred until ready.
    backup: FlowState,
    /// The membership `host` the running pullers were last spawned for. The drift
    /// signal for the periodic snapshot reconcile (ADR-0012 D2): a desired peer
    /// whose host moved (or whose flows are not running) is (re)spawned. `None`
    /// until a puller has been spawned for this ordinal.
    host: Option<String>,
}

impl PeerEntry {
    fn cold() -> Self {
        Self {
            reclaim: FlowState::cold(),
            backup: FlowState::cold(),
            host: None,
        }
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

/// Ties pullers to topology; owns the per-`(ordinal, flow)` retained watermarks.
#[derive(Clone)]
pub struct ReplicationSupervisor {
    inner: Arc<SupervisorInner>,
}

struct SupervisorInner {
    self_ordinal: String,
    network: Arc<dyn ReplicationNetwork>,
    store: ReplicatingCallStore,
    resolve: AddrResolver,
    config: PullerConfig,
    /// Replication observability, handed to each spawned puller.
    metrics: crate::metrics::B2buaMetrics,
    #[allow(dead_code)]
    clock: Clock,
    /// Cadence of the periodic snapshot reconcile (ADR-0012 D2).
    reconcile_period: Duration,
    /// Router command sink handed to every spawned puller (ADR-0011 X11 fail-back).
    /// `None` until [`set_repl_sink`](ReplicationSupervisor::set_repl_sink) wires it
    /// (the live `B2buaCore` does, before `start`); the sim/test supervisors leave
    /// it unset so pullers drive the store only.
    repl_tx: Mutex<Option<mpsc::UnboundedSender<crate::router::ReplCommand>>>,
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
}

impl ReplicationSupervisor {
    /// Build a supervisor for `self_ordinal`. Call [`start`](Self::start) to
    /// spawn the initial pullers + the topology-reconcile loop.
    pub fn new(
        self_ordinal: impl Into<String>,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        resolve: AddrResolver,
        clock: Clock,
        metrics: crate::metrics::B2buaMetrics,
    ) -> Self {
        Self::build(self_ordinal, network, store, resolve, clock, PullerConfig::default(), metrics)
    }

    /// Build with explicit puller backoff config (tests inject short backoff).
    /// Replication metrics default to a throwaway here; the live runner uses
    /// [`new`](Self::new) to share the worker's real `B2buaMetrics`.
    pub fn with_config(
        self_ordinal: impl Into<String>,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        resolve: AddrResolver,
        clock: Clock,
        config: PullerConfig,
    ) -> Self {
        Self::build(self_ordinal, network, store, resolve, clock, config, crate::metrics::B2buaMetrics::new())
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        self_ordinal: impl Into<String>,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        resolve: AddrResolver,
        clock: Clock,
        config: PullerConfig,
        metrics: crate::metrics::B2buaMetrics,
    ) -> Self {
        Self {
            inner: Arc::new(SupervisorInner {
                self_ordinal: self_ordinal.into(),
                network,
                store,
                resolve,
                config,
                metrics,
                clock,
                reconcile_period: DEFAULT_RECONCILE_PERIOD,
                repl_tx: Mutex::new(None),
                backup_enabled: AtomicBool::new(false),
                membership: Mutex::new(None),
                peers: Mutex::new(HashMap::new()),
                reconcile: Mutex::new(None),
                gate: Mutex::new(None),
            }),
        }
    }

    /// Wire the router command sink every puller forwards X11 fail-back commands
    /// to (reactive reclaim / bulk reclaim). Call **before** [`start`](Self::start)
    /// so the initial pullers pick it up; a re-spawned puller reads it too.
    pub fn set_repl_sink(&self, tx: mpsc::UnboundedSender<crate::router::ReplCommand>) {
        *self.inner.repl_tx.lock().unwrap() = Some(tx);
    }

    /// Spawn the Reclaim pullers per current peer (excluding self), keep them in
    /// step with membership, and run the backup-deferral gate that opens the
    /// Backup streams once the Reclaim flows are ready.
    ///
    /// Both the boot snapshot and every subsequent wakeup (delta, `Lagged`
    /// overflow, or periodic tick) flow through one idempotent
    /// [`reconcile_from_snapshot`](Self::reconcile_from_snapshot) — the shared
    /// [`topology::spawn_membership_reconcile`] driver owns the subscribe-before-
    /// snapshot ordering, the 5 s safety-net ticker, and the non-fatal `Lagged`
    /// handling (ADR-0012 D1/D2).
    pub fn start(&self, membership: Arc<dyn Membership>) {
        *self.inner.membership.lock().unwrap() = Some(membership.clone());
        let this = self.clone();
        let period = self.inner.reconcile_period;
        let handle = topology::spawn_membership_reconcile(membership.clone(), period, move |snapshot| {
            this.reconcile_from_snapshot(snapshot);
        });
        *self.inner.reconcile.lock().unwrap() = Some(handle);

        // Backup-deferral gate: open the Backup streams once the Reclaim flows
        // are all current-or-unreachable (ADR-0014 boot order).
        let this = self.clone();
        let gate = tokio::spawn(async move {
            this.run_backup_gate(membership).await;
        });
        *self.inner.gate.lock().unwrap() = Some(gate);
    }

    /// Wait until the Reclaim flows for every desired peer are ready, then latch
    /// `backup_enabled` and reconcile once so the Backup streams open. Exits
    /// immediately for a peerless node (nothing to back up). One-shot: returns
    /// once it has enabled backups; later peers get their Backup flow from the
    /// periodic reconcile (which honours the latched flag).
    ///
    /// **Event-driven, not a clock-poll.** It wakes the instant any Reclaim
    /// puller publishes a status change (its first `Noop` flips `current`), so the
    /// Backup streams open within the same scheduler turn — no full poll interval
    /// of latency. A `BACKUP_GATE_POLL` fallback bounds the wait against a lost
    /// wakeup or an unreachable peer whose hard timer must still latch it.
    async fn run_backup_gate(self, membership: Arc<dyn Membership>) {
        loop {
            if self.inner.backup_enabled.load(Ordering::SeqCst) {
                return;
            }
            let desired: Vec<Peer> = membership
                .snapshot()
                .into_iter()
                .filter(|p| p.ordinal != self.inner.self_ordinal)
                .collect();
            // A peerless node has no Backup streams to open — latch and stop.
            if desired.is_empty() {
                self.inner.backup_enabled.store(true, Ordering::SeqCst);
                return;
            }
            self.sync();
            let (ready, receivers) = {
                let peers = self.inner.peers.lock().unwrap();
                let ready = desired
                    .iter()
                    .all(|p| peers.get(&p.ordinal).is_some_and(|e| e.reclaim.ready()));
                let receivers: Vec<watch::Receiver<PullerStatus>> = desired
                    .iter()
                    .filter_map(|p| peers.get(&p.ordinal).and_then(|e| e.reclaim.status_rx.clone()))
                    .collect();
                (ready, receivers)
            };
            if ready {
                self.inner.backup_enabled.store(true, Ordering::SeqCst);
                self.reconcile_from_snapshot(membership.snapshot());
                return;
            }
            // Wait for ANY Reclaim status change (readiness re-check) or the
            // fallback poll, whichever comes first.
            await_any_change_or_poll(receivers, BACKUP_GATE_POLL).await;
        }
    }

    /// Reconcile the running pullers to a fresh membership `snapshot` (ADR-0012
    /// D1/D2). Acts only on drift: a desired peer whose Reclaim flow is **not
    /// running** or whose **host moved** is (re)spawned; once `backup_enabled` is
    /// latched the Backup flow is spawned/maintained the same way. A running
    /// puller whose ordinal is no longer desired is parked (both flows). An
    /// unchanged set produces no work.
    ///
    /// The whole reconcile holds the `peers` lock — including the `tokio::spawn`
    /// of each puller (no await points) — so a concurrent caller (the periodic
    /// loop vs. the backup gate) cannot interleave check-then-spawn and
    /// double-spawn a flow.
    fn reconcile_from_snapshot(&self, snapshot: Vec<Peer>) {
        let desired: Vec<Peer> = snapshot
            .into_iter()
            .filter(|p| p.ordinal != self.inner.self_ordinal)
            .collect();
        let backup_enabled = self.inner.backup_enabled.load(Ordering::SeqCst);

        let mut peers = self.inner.peers.lock().unwrap();

        for peer in &desired {
            let entry = peers
                .entry(peer.ordinal.clone())
                .or_insert_with(PeerEntry::cold);
            let drift = entry.host.as_deref() != Some(peer.host.as_str());
            entry.host = Some(peer.host.clone());

            // Reclaim always; Backup only once the gate has latched.
            if entry.reclaim.status_rx.is_none() || drift {
                self.spawn_flow(&mut entry.reclaim, peer, Partition::Pri);
            }
            // Under reactive-only takeover (ADR-0014) there is **no** eager
            // death-triggered takeover: a quiescent failed-over dialog is
            // recovered only when the rebooting primary reclaims it (the Reclaim
            // flow, smoothed), or earlier on in-dialog traffic the LB reroutes to
            // a survivor (reactive takeover). A quiescent call on a permanently-
            // lost node dies after the keepalive slack — the deliberate trade for
            // killing the eager-takeover stale-CSeq storm (ADR-0014 §13).
            if backup_enabled && (entry.backup.status_rx.is_none() || drift) {
                self.spawn_flow(&mut entry.backup, peer, Partition::Bak);
            }
        }

        // Park: any peer whose ordinal departed the desired set (both flows).
        let desired_ords: std::collections::HashSet<&str> =
            desired.iter().map(|p| p.ordinal.as_str()).collect();
        for (ord, entry) in peers.iter_mut() {
            if !desired_ords.contains(ord.as_str()) {
                entry.park();
            }
        }
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
        let puller = match self.inner.repl_tx.lock().unwrap().clone() {
            Some(tx) => puller.with_repl_sink(tx),
            None => puller,
        };
        let (cancel_tx, cancel_rx) = watch::channel(false);
        flow.status_rx = Some(status_rx);
        flow.cancel_tx = Some(cancel_tx);

        tokio::spawn(async move {
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
        if let Some(h) = self.inner.reconcile.lock().unwrap().take() {
            h.abort();
        }
        if let Some(h) = self.inner.gate.lock().unwrap().take() {
            h.abort();
        }
        let mut peers = self.inner.peers.lock().unwrap();
        for entry in peers.values_mut() {
            entry.park();
        }
    }

    /// Fold every running puller's published status into the retained map. Tests
    /// call this (or any retained read) after advancing the clock.
    fn sync(&self) {
        let mut peers = self.inner.peers.lock().unwrap();
        for entry in peers.values_mut() {
            entry.reclaim.absorb();
            entry.backup.absorb();
        }
    }

    /// Is `peer`'s **Reclaim** flow current (its sticky current flag set)? Folds
    /// live status first.
    pub fn is_current(&self, peer: &str) -> bool {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.reclaim.current)
            .unwrap_or(false)
    }

    /// The set of peer ordinals this node is currently responsible for (the
    /// desired membership snapshot minus self). `None` until membership is wired
    /// by [`start`](Self::start) — pre-start no pullers are spawned, so the
    /// retained map is empty and the gates' `None`-means-no-filter is harmless.
    ///
    /// The readiness gates ([`all_current`](Self::all_current) /
    /// [`all_bootstrapped`](Self::all_bootstrapped)) filter the retained `peers`
    /// map through this so a peer that has **left** the desired membership cannot
    /// pin readiness NotReady. The departed entry is **retained** (its watermark
    /// survives for a warm resume if the ordinal returns, X5) but it is no longer
    /// our responsibility — exactly as the backup-deferral gate already treats
    /// `desired` (`run_backup_gate`). Without this filter a cold double-restart
    /// that briefly sees a peer at a **stale** IP then loses it (both NotReady →
    /// `publishNotReadyAddresses:false` empties the EndpointSlice) parks that
    /// entry `{current:false, bootstrap_complete:false, ever_connected:false}`
    /// forever and wedges the node NotReady — no puller is left to fire the
    /// bootstrap hard timer that would otherwise mark it complete (ADR-0012 D2).
    fn desired_ordinals(&self) -> Option<std::collections::HashSet<String>> {
        self.inner.membership.lock().unwrap().as_ref().map(|m| {
            m.snapshot()
                .into_iter()
                .filter(|p| p.ordinal != self.inner.self_ordinal)
                .map(|p| p.ordinal)
                .collect()
        })
    }

    /// Are ALL **desired** peers' **Reclaim** flows current — or unreachable? (S7
    /// readiness gate.) Empty set → `true`. Peers that have left the desired
    /// membership are excluded ([`desired_ordinals`](Self::desired_ordinals)) so a
    /// parked, departed entry cannot pin readiness. A peer that is bootstrap-
    /// complete only via the hard timer and was **never reached** does NOT block
    /// readiness (per Decision 4 a node must boot and serve even when peers are
    /// unreachable). A reachable-then-blipped peer **still desired** keeps the
    /// strict gate (sticky `current`).
    pub fn all_current(&self) -> bool {
        self.sync();
        let desired = self.desired_ordinals();
        self.inner
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|(ord, _)| desired.as_ref().is_none_or(|d| d.contains(ord.as_str())))
            .all(|(_, e)| e.reclaim.ready())
    }

    /// Has `peer`'s **Reclaim** bootstrap completed (first `Noop`, hard timer, or
    /// warm resume)? Folds live status first. Sticky across Park (X5).
    pub fn bootstrap_complete(&self, peer: &str) -> bool {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.reclaim.bootstrap_complete)
            .unwrap_or(false)
    }

    /// Are ALL **desired** peers' **Reclaim** flows bootstrap-complete? (S7
    /// readiness gate.) Empty set → `true`. Peers that have left the desired
    /// membership are excluded ([`desired_ordinals`](Self::desired_ordinals)) so a
    /// parked, departed entry cannot pin readiness.
    pub fn all_bootstrapped(&self) -> bool {
        self.sync();
        let desired = self.desired_ordinals();
        self.inner
            .peers
            .lock()
            .unwrap()
            .iter()
            .filter(|(ord, _)| desired.as_ref().is_none_or(|d| d.contains(ord.as_str())))
            .all(|(_, e)| e.reclaim.bootstrap_complete)
    }

    /// The retained **Reclaim** watermark for `peer` (test introspection).
    pub fn watermark(&self, peer: &str) -> Watermark {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.reclaim.watermark)
            .unwrap_or_else(|| Watermark::new(0, 0))
    }

    /// The retained watermark for a specific `(peer, flow)` (test introspection
    /// into the Backup flow, which the readiness accessors do not expose).
    pub fn flow_watermark(&self, peer: &str, partition: Partition) -> Watermark {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.flow(partition).watermark)
            .unwrap_or_else(|| Watermark::new(0, 0))
    }

    /// Whether the **Reclaim** puller is currently running (not Parked) for `peer`.
    pub fn is_running(&self, peer: &str) -> bool {
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.reclaim.status_rx.is_some())
            .unwrap_or(false)
    }

    /// Await `peer`'s **Reclaim** flow becoming current (sticky), folding live
    /// status as it ticks. Returns once current; the caller drives the clock.
    pub async fn await_current(&self, peer: &str) {
        loop {
            if self.is_current(peer) {
                return;
            }
            let rx = self
                .inner
                .peers
                .lock()
                .unwrap()
                .get(peer)
                .and_then(|e| e.reclaim.status_rx.clone());
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

/// Wait until **any** of `receivers` publishes a status change, or `fallback`
/// elapses — whichever first. Used by the backup-deferral gate so it re-checks
/// Reclaim readiness the instant a puller advances, without a full poll interval
/// of latency. A short-lived forwarder task per receiver (aborted on return)
/// avoids needing a `select_all` combinator; the gate is one-shot, so this runs
/// only during boot until the Backup streams open.
async fn await_any_change_or_poll(
    receivers: Vec<watch::Receiver<PullerStatus>>,
    fallback: Duration,
) {
    if receivers.is_empty() {
        tokio::time::sleep(fallback).await;
        return;
    }
    let (tx, mut rx) = mpsc::channel::<()>(1);
    let handles: Vec<tokio::task::JoinHandle<()>> = receivers
        .into_iter()
        .map(|mut r| {
            let tx = tx.clone();
            tokio::spawn(async move {
                if r.changed().await.is_ok() {
                    let _ = tx.try_send(());
                }
            })
        })
        .collect();
    drop(tx);
    tokio::select! {
        _ = tokio::time::sleep(fallback) => {}
        _ = rx.recv() => {}
    }
    for h in handles {
        h.abort();
    }
}
