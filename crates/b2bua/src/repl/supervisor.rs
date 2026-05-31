//! [`ReplicationSupervisor`] — ties per-peer [`Puller`]s to cluster topology
//! (migration slice S5).
//!
//! On start it `snapshot`s membership and spawns a [`Puller`] per peer (skipping
//! self). It then subscribes to `changes()` and reconciles:
//! - `Added` → spawn/Connecting (or un-Park: reconnect from the retained W).
//! - `Removed` → `Parked`: interrupt the puller, **retain W forever** keyed by
//!   ordinal.
//! - `AddressChanged` → reconnect to the new addr from the retained W.
//!
//! ## Watermark retention per ordinal
//! The authoritative watermark + current flag live in the supervisor, keyed by
//! ordinal, and **survive** Park/disconnect/re-add. A running puller publishes
//! its progress on a `watch` channel the supervisor mirrors into the retained
//! map; when a puller is re-spawned (re-add / address change) it is seeded from
//! the retained W so it resumes rather than re-pulling from scratch.
//!
//! ## Introspection (for tests / S7 readiness)
//! [`is_current`](ReplicationSupervisor::is_current) /
//! [`all_current`](ReplicationSupervisor::all_current) expose the sticky current
//! flag; [`await_current`](ReplicationSupervisor::await_current) parks until a
//! peer is current; [`watermark`](ReplicationSupervisor::watermark) reads the
//! retained W. S7's readiness state machine consumes `all_current`.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use repl_net::frame::Watermark;
use repl_net::transport::ReplicationNetwork;
use sip_clock::Clock;
use tokio::sync::watch;
use topology::{MemberDelta, Membership, Peer};

use super::puller::{Puller, PullerConfig, PullerStatus};
use super::ReplicatingCallStore;

/// Resolves a [`Peer`] to its replication [`SocketAddr`] (ordinal+host+config →
/// repl addr). For sim tests this maps ordinal/host → the peer's repl addr.
pub type AddrResolver = Arc<dyn Fn(&Peer) -> SocketAddr + Send + Sync>;

/// Per-ordinal retained replication state — survives Park/disconnect/re-add.
struct PeerEntry {
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
    /// puller hit the terminal bootstrap `Noop`, the hard timer fired, or it
    /// resumed warm. S7 readiness consumes `all_bootstrapped`.
    bootstrap_complete: bool,
    /// Highest reset generation absorbed from any puller for this ordinal. A
    /// puller bumps its `reset_gen` when the server pushes `ResetToBootstrap`
    /// (watermark forced back to `(0,0)`); a higher value here means we must pull
    /// the retained watermark DOWN so a respawn re-bootstraps instead of resuming
    /// the now-invalid high W.
    reset_gen: u64,
    /// Sticky: this peer was reached by a successful `connect` at least once.
    /// An unreachable peer (never connected) that goes bootstrap-complete only by
    /// the hard timer must NOT pin readiness NotReady (Decision 4 — liveness).
    ever_connected: bool,
}

impl PeerEntry {
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

    /// Fold the puller's latest published status into the retained copy. Current
    /// + ever-connected are sticky (OR). A `reset_gen` advance means the server
    /// forced a re-bootstrap: pull the watermark DOWN to `(0,0)` and clear
    /// bootstrap-complete so a respawn re-bootstraps. Otherwise bootstrap-complete
    /// is sticky and the watermark only advances.
    fn absorb(&mut self) {
        if let Some(rx) = &self.status_rx {
            let s = *rx.borrow();
            self.current |= s.current;
            self.ever_connected |= s.ever_connected;
            if s.reset_gen > self.reset_gen {
                // Server-driven reset: honour the pull-down, don't sticky-OR it away.
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
}

/// Ties pullers to topology; owns the per-ordinal retained watermarks.
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
    /// `ordinal → PeerEntry`.
    peers: Mutex<HashMap<String, PeerEntry>>,
    /// The topology-reconcile loop's handle, retained so [`shutdown`] can abort
    /// it. Without this the loop outlives `crash()`/`shutdown()` (it holds an
    /// `Arc<SupervisorInner>`) and keeps spawning pullers against a dead node on
    /// later membership deltas — a task/memory leak + double-replication.
    ///
    /// [`shutdown`]: ReplicationSupervisor::shutdown
    reconcile: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
                peers: Mutex::new(HashMap::new()),
                reconcile: Mutex::new(None),
            }),
        }
    }

    /// Spawn a puller per current peer (excluding self), then spawn the
    /// topology-reconcile loop. Idempotent-ish: call once after construction.
    pub fn start(&self, membership: Arc<dyn Membership>) {
        // Subscribe BEFORE the snapshot so no delta between snapshot and
        // subscribe is lost (no backfill on `changes()`).
        let mut changes = membership.changes();
        for peer in membership.snapshot() {
            if peer.ordinal != self.inner.self_ordinal {
                self.spawn_puller(&peer);
            }
        }
        let this = self.clone();
        let handle = tokio::spawn(async move {
            loop {
                match changes.recv().await {
                    Ok(MemberDelta::Added(peer)) => {
                        if peer.ordinal != this.inner.self_ordinal {
                            this.spawn_puller(&peer);
                        }
                    }
                    Ok(MemberDelta::AddressChanged(peer)) => {
                        if peer.ordinal != this.inner.self_ordinal {
                            // Reconnect to the new addr from the retained W.
                            this.spawn_puller(&peer);
                        }
                    }
                    Ok(MemberDelta::Removed(ordinal)) => {
                        this.park(&ordinal);
                    }
                    // Lagged/closed: re-sync from a fresh snapshot would go here;
                    // for S5 sim tests the channel never lags.
                    Err(_) => return,
                }
            }
        });
        *self.inner.reconcile.lock().unwrap() = Some(handle);
    }

    /// Spawn (or re-spawn) a puller for `peer`, seeded from its retained W. If a
    /// puller is already running it is cancelled first (its W is absorbed).
    fn spawn_puller(&self, peer: &Peer) {
        let addr = (self.inner.resolve)(peer);
        let start_w = {
            let mut peers = self.inner.peers.lock().unwrap();
            let entry = peers
                .entry(peer.ordinal.clone())
                .or_insert_with(PeerEntry::cold);
            // Cancel any running puller and fold its progress into retained.
            entry.absorb();
            if let Some(tx) = entry.cancel_tx.take() {
                let _ = tx.send(true);
            }
            entry.status_rx = None;
            entry.watermark
        };

        let (puller, status_rx) = Puller::new(
            peer.ordinal.clone(),
            self.inner.self_ordinal.clone(),
            addr,
            self.inner.network.clone(),
            self.inner.store.clone(),
            self.inner.config,
            start_w,
            self.inner.metrics.clone(),
        );
        let (cancel_tx, cancel_rx) = watch::channel(false);

        {
            let mut peers = self.inner.peers.lock().unwrap();
            let entry = peers.get_mut(&peer.ordinal).unwrap();
            entry.status_rx = Some(status_rx);
            entry.cancel_tx = Some(cancel_tx);
        }

        tokio::spawn(async move {
            puller.run(cancel_rx).await;
        });
    }

    /// Shut every running puller down (Park them all): interrupt each puller via
    /// its existing cancel handle and drop its status receiver. Reuses the
    /// per-peer Park path; the retained watermarks/current flags survive (as on
    /// any Park). Used by [`B2buaCore::abort`](crate::B2buaCore::abort) to stop a
    /// crashed worker's pullers without changing replication behaviour.
    pub fn shutdown(&self) {
        // Stop reacting to membership deltas FIRST: abort the reconcile loop so it
        // can't spawn a new puller against this (about-to-be-dead) node after we
        // park the existing ones. Also frees the loop's `Arc<SupervisorInner>`.
        if let Some(h) = self.inner.reconcile.lock().unwrap().take() {
            h.abort();
        }
        let ordinals: Vec<String> = self.inner.peers.lock().unwrap().keys().cloned().collect();
        for ordinal in ordinals {
            self.park(&ordinal);
        }
    }

    /// Park the puller for `ordinal`: interrupt it, retain W + current forever.
    fn park(&self, ordinal: &str) {
        let mut peers = self.inner.peers.lock().unwrap();
        if let Some(entry) = peers.get_mut(ordinal) {
            entry.absorb();
            if let Some(tx) = entry.cancel_tx.take() {
                let _ = tx.send(true);
            }
            entry.status_rx = None;
        }
    }

    /// Fold every running puller's published status into the retained map. Tests
    /// call this (or any retained read) after advancing the clock.
    fn sync(&self) {
        let mut peers = self.inner.peers.lock().unwrap();
        for entry in peers.values_mut() {
            entry.absorb();
        }
    }

    /// Is `peer` current (its sticky current flag set)? Folds live status first.
    pub fn is_current(&self, peer: &str) -> bool {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.current)
            .unwrap_or(false)
    }

    /// Are ALL known peers current — or unreachable? (S7 readiness gate.) Empty
    /// set → `true`. A peer that is bootstrap-complete only via the hard timer
    /// and was **never reached** (`!ever_connected`) does NOT block readiness:
    /// per Decision 4 a node must boot and serve even when peers are unreachable.
    /// A reachable-then-blipped peer keeps the strict gate (sticky `current`).
    pub fn all_current(&self) -> bool {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .values()
            .all(|e| e.current || (e.bootstrap_complete && !e.ever_connected))
    }

    /// Has `peer`'s bootstrap completed (terminal `Noop`, hard timer, or warm
    /// resume)? Folds live status first. Sticky across Park (X5).
    pub fn bootstrap_complete(&self, peer: &str) -> bool {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.bootstrap_complete)
            .unwrap_or(false)
    }

    /// Are ALL known peers bootstrap-complete? (S7 readiness gate — true when
    /// every reachable peer hit terminal-`Noop` OR the hard timer fired.) Empty
    /// set → `true` (a node with no peers is immediately re-hydrated).
    pub fn all_bootstrapped(&self) -> bool {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .values()
            .all(|e| e.bootstrap_complete)
    }

    /// The retained watermark for `peer` (test introspection).
    pub fn watermark(&self, peer: &str) -> Watermark {
        self.sync();
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.watermark)
            .unwrap_or_else(|| Watermark::new(0, 0))
    }

    /// Whether a puller is currently running (not Parked) for `peer`.
    pub fn is_running(&self, peer: &str) -> bool {
        self.inner
            .peers
            .lock()
            .unwrap()
            .get(peer)
            .map(|e| e.status_rx.is_some())
            .unwrap_or(false)
    }

    /// Await `peer` becoming current (sticky), folding live status as it ticks.
    /// Returns once current; the caller drives the clock between polls.
    pub async fn await_current(&self, peer: &str) {
        loop {
            if self.is_current(peer) {
                return;
            }
            // Grab the live receiver and await a status change, then re-check.
            let rx = self
                .inner
                .peers
                .lock()
                .unwrap()
                .get(peer)
                .and_then(|e| e.status_rx.clone());
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
