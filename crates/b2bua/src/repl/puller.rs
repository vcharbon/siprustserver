//! [`Puller`] — the per-`(peer, flow)` replication **client FSM** (ADR-0014
//! §Stream topology).
//!
//! One `Puller` drives the client side for ONE peer ordinal on ONE **flow**
//! (`partition`): it opens a single-flow connection, issues a single
//! [`Frame::PullRequest`], and applies the long-lived stream of `Data` / `Noop`
//! the server pushes, reconnecting with exponential backoff across cuts. The
//! supervisor runs **two** pullers per peer — a **Reclaim** flow (`partition =
//! Pri`, arms timers / reclaims) and a **Backup** flow (`partition = Bak`, no
//! timers, metrics-only). The watermark `W` each resumes from is **owned by the
//! supervisor** (retained per `(ordinal, flow)` across Park/disconnect/re-add)
//! and passed in at construction.
//!
//! ## One stream, scan-then-tail (no second request)
//! The server treats a `PullRequest{ partition, since }` as the whole contract:
//! - `since == (0,0)` (cold) ⇒ it store-scans the flow's keyspace, streaming each
//!   live body as `Data` **all stamped `at = W = head@scan-start`**, then tails
//!   from `W` on the SAME socket. The **first catch-up `Noop`** ends the
//!   bootstrap — there is no terminal bootstrap `Noop` and no second request.
//! - `since > (0,0)` (warm) ⇒ it tails directly from `since`.
//!
//! ## No client-side apply-gate — `(p,b)` is the only idempotency
//! The puller applies **every** `Data` under the ADR-0014 `(p,b)` version-vector
//! rule; a re-delivered/stale frame is rejected by dominance, not by a watermark
//! compare. Dropping the old `at <= W → skip` gate is what fixes the
//! bootstrap-frame collision (every bootstrap frame shares `at = W`, so the gate
//! used to admit only the first and silently drop the rest — the ~203/3000
//! re-hydration cliff).
//!
//! ## Watermark advances only on `Noop` and post-bootstrap tail `Data.at`
//! `W` is a pure **changelog position**, never the `(p,b)` vector. It advances on
//! a `Noop(head)` and on a post-bootstrap tail `Data.at` — **never** on a
//! bootstrap frame (they share `at = W`; advancing mid-scan then disconnecting
//! would skip the un-sent remainder). The per-run `bootstrapped` flag flips on
//! the first `Noop`; before it, frames apply ungated and do not move `W`.
//!
//! ## `ApplyMode = f(flow, bootstrapped)`
//! - **Backup** flow (`Bak`) — always *apply-unless-dominated* (a Forward
//!   primary→backup update is the same rule as a bootstrap import).
//! - **Reclaim** flow (`Pri`) — *apply-unless-dominated* before the first `Noop`
//!   (bulk bootstrap recovery), then the *Reverse* rule after (`p_in == sp &&
//!   b_in > sb`: a backup reverse-flushed one of our calls).
//!
//! ## FSM
//! ```text
//! Connecting → (send PullRequest) → Streaming{bootstrapped} → Backoff → Connecting
//! ```
//! - connect ok → send one `PullRequest{ partition, since = retained W }`.
//! - `Data{at}` → apply under `(p,b)`; if `bootstrapped`, `W = at` (+ Reclaim
//!   straggler [`ReclaimCall`](ReplCommand::ReclaimCall)).
//! - `Noop{at}` → `current = true` (sticky); first one marks bootstrap-complete,
//!   flips `bootstrapped`, and (Reclaim flow) fires the bulk
//!   [`ReclaimAll`](ReplCommand::ReclaimAll); advance `W` if greater.
//! - `ResetToBootstrap` → discard `W`, clear bootstrap-complete, bump
//!   `reset_gen`, disconnect (reconnect re-bootstraps from `(0,0)`).
//! - recv `None` / send `Err` → `Backoff` (RETAIN W).
//! - `Backoff`: `sleep(min(init·2^attempt, max))` + a select on cancel (a plain
//!   `sleep`+`select`, **not** a `DelayQueue` — CLAUDE.md aliasing hazard).
//!
//! ## Bootstrap hard timer (X5 / Decision 4 — liveness over completeness)
//! A cold puller arms one absolute deadline `now + bootstrap_hard_timeout_ms`.
//! While bootstrap is outstanding the connect wait **and** the first-`Noop` wait
//! race it; if it fires the puller goes **bootstrap-complete (best-effort)** so
//! the node boots and serves even when a peer is unreachable or pathologically
//! slow. Unlike the old design it does **not** abandon the connection — there is
//! no apply-gate collision to avoid, so it keeps streaming on the same socket and
//! the real first `Noop` (when it arrives) completes the bootstrap for real.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Frame, Op, Partition, Watermark};
use repl_net::transport::{ReplicationConnection, ReplicationNetwork};
use tokio::sync::{mpsc, watch};
use topology::Peer;

use super::{AddrResolver, ReplicatingCallStore};
use crate::router::ReplCommand;
use crate::store::{partition_of, CallStore, PartitionRole, PutOpts};

/// Default backoff floor / ceiling (ms). Exposed via [`PullerConfig`] for tests.
const DEFAULT_BACKOFF_INIT_MS: u64 = 100;
const DEFAULT_BACKOFF_MAX_MS: u64 = 30_000;
/// Default bootstrap hard-timeout (ms): the upper bound on how long the puller
/// waits — for a reachable connection AND for the first catch-up `Noop` — before
/// declaring bootstrap-complete best-effort (X5 / Decision 4 — liveness over
/// completeness). The connection is NOT dropped on expiry; it keeps streaming.
const DEFAULT_BOOTSTRAP_HARD_TIMEOUT_MS: u64 = 10_000;
/// Protocol version stamped on the `PullRequest`. **v3** (ADR-0014 stream split):
/// the wire message set was trimmed to the four-frame `{PullRequest, Data, Noop,
/// ResetToBootstrap}` form, `PullRequest` lost its `mode`/`chunk` fields (one
/// socket = one flow, scan-then-tail), and `Op` collapsed to `{Put, Delete}`. A
/// v3 node cannot interpret a v2 peer's frames — pre-production, so no compat shim.
const PROTO_VER: u16 = 3;

/// How an inbound `Data` frame is reconciled into the local store
/// ([`Puller::apply_to_store`]). Selected per frame from `(flow, bootstrapped)`:
/// the bulk pre-seed (recovering our own partition) and a Forward primary→backup
/// update both take the replica unless dominated; the Reclaim-flow steady tail
/// applies the direction-aware Reverse rule.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ApplyMode {
    /// Apply-unless-dominated — bootstrap recovery or a Forward primary→backup
    /// update (the follower defers to the authority).
    ForwardOrBootstrap,
    /// Reclaim-flow steady tail — apply the ADR-0014 Reverse `(p,b)` rule.
    Reverse,
}

/// Backoff knobs for a puller (tests inject short values).
#[derive(Clone, Copy, Debug)]
pub struct PullerConfig {
    /// Backoff floor in ms (`init` in `min(init·2^attempt, max)`).
    pub backoff_init_ms: u64,
    /// Backoff ceiling in ms.
    pub backoff_max_ms: u64,
    /// Bootstrap **liveness** deadline in ms (X5): the budget for reaching a peer
    /// AND for the first catch-up `Noop`. On expiry the puller marks **bootstrap-
    /// complete (best-effort)** so the node boots and serves regardless of an
    /// unreachable or pathologically-slow peer — WITHOUT dropping the connection,
    /// which keeps streaming until the real `Noop` arrives. Kept short — it gates
    /// readiness, never correctness (the `(p,b)` merge admits late frames).
    pub bootstrap_hard_timeout_ms: u64,
}

impl Default for PullerConfig {
    fn default() -> Self {
        Self {
            backoff_init_ms: DEFAULT_BACKOFF_INIT_MS,
            backoff_max_ms: DEFAULT_BACKOFF_MAX_MS,
            bootstrap_hard_timeout_ms: DEFAULT_BOOTSTRAP_HARD_TIMEOUT_MS,
        }
    }
}

impl PullerConfig {
    /// Test-tier config: short backoff + a 2 s bootstrap hard timeout, so a few
    /// paused-clock advances trip the relevant deadline deterministically. ONE
    /// definition — these values are timing-load-bearing under the paused clock
    /// (tests advance exactly to a deadline), and the previous per-harness
    /// copies (ha-harness, b2bua test_support, s5) had already drifted.
    pub fn fast_test() -> Self {
        Self {
            backoff_init_ms: 100,
            backoff_max_ms: 1_000,
            bootstrap_hard_timeout_ms: 2_000,
        }
    }
}

/// Shared, observable per-`(peer, flow)` puller status. The supervisor reads
/// `current` for readiness (scoped to the **Reclaim** flow) and the retained
/// `watermark`.
#[derive(Clone, Copy, Debug)]
pub struct PullerStatus {
    /// Sticky current flag — set on the first `Noop`, never cleared.
    pub current: bool,
    /// The retained watermark (advances on Noop / post-bootstrap tail). The
    /// supervisor owns the authoritative copy keyed by `(ordinal, flow)`; the
    /// puller publishes its progress here so a Park/re-add resumes from it.
    pub watermark: Watermark,
    /// Sticky **bootstrap-complete** flag (X5 / Decision 4). Set when the first
    /// catch-up `Noop` arrives OR the bootstrap hard timer fires (best-effort) OR
    /// the puller resumes warm (`W > (0,0)` — no bootstrap needed). Cleared only
    /// by a `ResetToBootstrap`. S7 readiness consumes this.
    pub bootstrap_complete: bool,
    /// Monotonic reset generation: bumped each time the server pushes
    /// `ResetToBootstrap` (watermark forced back to `(0,0)`). Lets the supervisor
    /// distinguish "never advanced past `(0,0)`" from "was just reset" and pull
    /// the retained watermark down accordingly (survives the `watch` channel's
    /// last-value-wins coalescing of a reset-then-advance).
    pub reset_gen: u64,
    /// Set the first time `connect` succeeds for this peer. Readiness uses it so
    /// an unreachable peer (hard-timer bootstrap-complete, never connected) does
    /// not pin the node NotReady.
    pub ever_connected: bool,
}

/// What ended a single `run_once` connection attempt — drives the supervisor's
/// retention + the puller's own reconnect.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RunOutcome {
    /// Connect failed → Backoff (attempt++).
    ConnectFailed,
    /// Connection cut after a successful connect → Backoff (retain W).
    Disconnected,
    /// The cancel signal fired (Park / shutdown) → stop running.
    Cancelled,
}

/// Result of racing a future against the cancel signal + the bootstrap hard
/// deadline ([`Puller::select_with_deadline`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectOutcome {
    /// The wrapped future finished first.
    Completed,
    /// The bootstrap hard deadline fired first.
    Deadline,
    /// The cancel signal fired (Park / shutdown).
    Cancelled,
}

/// Drives the client side for ONE `(peer, flow)`. Cheap to construct; `run` loops
/// until cancelled. The watermark + current flag are published on a `watch`
/// channel the supervisor reads.
pub struct Puller {
    /// The peer this puller replicates from (its `ordinal` identity + `host`).
    peer: Peer,
    /// This node's ordinal (apply-target keyspace resolution).
    self_ordinal: String,
    /// The **flow** this puller serves: `Pri` = Reclaim (arm timers / reclaim),
    /// `Bak` = Backup (no timers, metrics-only).
    partition: Partition,
    /// Resolves `peer` to a [`SocketAddr`] **fresh on every connect attempt**
    /// (ADR-0012 D3) — so a restarted peer's new IP is picked up on reconnect
    /// without waiting on a membership delta.
    resolve: AddrResolver,
    network: Arc<dyn ReplicationNetwork>,
    /// Apply target — the local store.
    store: ReplicatingCallStore,
    config: PullerConfig,
    /// Published status (current flag + watermark) the supervisor observes.
    status_tx: watch::Sender<PullerStatus>,
    /// Replication observability (inbound apply counters + backup-held gauge).
    metrics: crate::metrics::B2buaMetrics,
    /// Sink to the router for fail-back commands (ADR-0011 X11 / ADR-0014): a
    /// **reactive reclaim** when a backup reverse-flushes one of our calls
    /// (Reclaim-flow tail `Put`) and the bulk reclaim on bootstrap completion.
    /// `None` outside a live `B2buaCore` (the sim/unit puller tests drive the
    /// store directly), so the existing constructors stay source-compatible.
    repl_tx: Option<mpsc::UnboundedSender<ReplCommand>>,
}

impl Puller {
    /// Build a puller for `peer` on `partition` (the flow), resolving its address
    /// per-connect via `resolve` (ADR-0012 D3), seeded from `start_w` (the
    /// supervisor's retained watermark — `(0,0)` for a cold/never-seen peer).
    /// Returns the puller plus a [`watch::Receiver`] the supervisor reads.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        peer: Peer,
        self_ordinal: impl Into<String>,
        partition: Partition,
        resolve: AddrResolver,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        config: PullerConfig,
        start_w: Watermark,
        metrics: crate::metrics::B2buaMetrics,
    ) -> (Self, watch::Receiver<PullerStatus>) {
        // A warm resume (W > (0,0)) needs no bootstrap — mark it complete up
        // front so readiness doesn't wait on a bootstrap that won't happen.
        let warm = start_w != Watermark::new(0, 0);
        let (status_tx, status_rx) = watch::channel(PullerStatus {
            current: false,
            watermark: start_w,
            bootstrap_complete: warm,
            reset_gen: 0,
            ever_connected: false,
        });
        (
            Self {
                peer,
                self_ordinal: self_ordinal.into(),
                partition,
                resolve,
                network,
                store,
                config,
                status_tx,
                metrics,
                repl_tx: None,
            },
            status_rx,
        )
    }

    /// Attach the router command sink (ADR-0011 X11 fail-back). Builder so the
    /// existing [`new`](Self::new)/[`new_at`](Self::new_at) callers (sim + unit
    /// tests, which assert on the store directly) are unchanged.
    pub fn with_repl_sink(mut self, tx: mpsc::UnboundedSender<ReplCommand>) -> Self {
        self.repl_tx = Some(tx);
        self
    }

    /// Convenience constructor for a puller that always connects to a **fixed**
    /// `peer_addr` (a resolver-less caller / tests with a concrete bound socket).
    /// Wraps `peer_addr` in a constant [`FnPeerResolver`].
    #[allow(clippy::too_many_arguments)]
    pub fn new_at(
        peer_ordinal: impl Into<String>,
        self_ordinal: impl Into<String>,
        partition: Partition,
        peer_addr: SocketAddr,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        config: PullerConfig,
        start_w: Watermark,
        metrics: crate::metrics::B2buaMetrics,
    ) -> (Self, watch::Receiver<PullerStatus>) {
        let ordinal = peer_ordinal.into();
        let resolve: AddrResolver =
            Arc::new(super::FnPeerResolver(move |_: &Peer| peer_addr));
        Self::new(
            Peer::new(ordinal, peer_addr.to_string()),
            self_ordinal,
            partition,
            resolve,
            network,
            store,
            config,
            start_w,
            metrics,
        )
    }

    /// The peer ordinal this puller serves.
    pub fn peer_ordinal(&self) -> &str {
        &self.peer.ordinal
    }

    /// Whether this is the **Reclaim** flow (`partition = Pri`) — the only flow
    /// that arms timers and signals reclaim to the router.
    fn is_reclaim(&self) -> bool {
        self.partition == Partition::Pri
    }

    /// Stream-kind label for `b2bua_repl_applied_total{flow,…}`: `recovery` = the
    /// Pri/Reclaim flow (our own calls pulled back from a peer's backup), `backup`
    /// = the Bak flow (a peer's calls we hold as backup).
    fn flow_label(&self) -> &'static str {
        if self.is_reclaim() { "recovery" } else { "backup" }
    }

    /// Resolve the peer's address (fresh) and open a connection. `None` if the
    /// address is unresolvable right now or the connect fails — the caller treats
    /// either as a failed connect (→ Backoff, then retry/re-resolve).
    async fn try_connect(&self) -> Option<Box<dyn ReplicationConnection>> {
        let addr = self.resolve.resolve(&self.peer).await?;
        self.network.connect(addr).await.ok()
    }

    /// Run the FSM until `cancel` flips to `true` (Park / shutdown). `cancel` is
    /// a `watch` so the supervisor can interrupt a parked-out puller atomically.
    pub async fn run(self, mut cancel: watch::Receiver<bool>) {
        let mut attempt: u32 = 0;
        // Arm the bootstrap hard deadline iff we start cold. A warm resume is
        // already `bootstrap_complete` (set in `new`), so no deadline is needed.
        let mut deadline: Option<tokio::time::Instant> =
            if self.status_tx.borrow().bootstrap_complete {
                None
            } else {
                Some(
                    tokio::time::Instant::now()
                        + Duration::from_millis(self.config.bootstrap_hard_timeout_ms),
                )
            };

        loop {
            if *cancel.borrow() {
                return;
            }
            match self.run_once(&mut cancel, &mut deadline).await {
                RunOutcome::Cancelled => return,
                RunOutcome::ConnectFailed | RunOutcome::Disconnected => {
                    // ---- Backoff{attempt} ---- retain W (status already holds it).
                    attempt = attempt.saturating_add(1);
                    let ms = self.backoff_ms(attempt);
                    // The hard timer must keep ticking through backoff so an
                    // unreachable peer still becomes bootstrap-complete on time.
                    let fire_hard = self.select_with_deadline(
                        tokio::time::sleep(Duration::from_millis(ms)),
                        &mut cancel,
                        deadline,
                    );
                    match fire_hard.await {
                        SelectOutcome::Cancelled => return,
                        SelectOutcome::Deadline => {
                            self.mark_bootstrap_complete();
                            deadline = None;
                        }
                        SelectOutcome::Completed => {}
                    }
                    // → Connecting (loop).
                }
            }
            // Once bootstrap is complete, drop the deadline so later reconnects
            // (warm) don't re-arm it.
            if self.status_tx.borrow().bootstrap_complete {
                deadline = None;
            }
        }
    }

    /// Mark this puller **bootstrap-complete** (sticky). Called on the first
    /// catch-up `Noop` and on the hard-timer firing (best-effort).
    fn mark_bootstrap_complete(&self) {
        self.status_tx.send_modify(|s| s.bootstrap_complete = true);
    }

    /// Signal the router to **bulk-reclaim** every `pri:{self}` body — materialise
    /// each into the live serving map + re-arm its timers, with the keepalive
    /// backlog smoothed (ADR-0014 §4). Fired by the **Reclaim** flow on bootstrap
    /// completion (first `Noop` or best-effort hard-timer). Idempotent
    /// (`materialize_if_absent`), so a partial pass plus a later one self-heal.
    fn signal_reclaim_all(&self) {
        if let Some(tx) = &self.repl_tx {
            let _ = tx.send(ReplCommand::ReclaimAll);
        }
    }

    /// Race `fut` against the cancel signal and (optionally) the bootstrap hard
    /// deadline. Returns which fired. A `None` deadline means "no hard timer".
    async fn select_with_deadline<F: std::future::Future>(
        &self,
        fut: F,
        cancel: &mut watch::Receiver<bool>,
        deadline: Option<tokio::time::Instant>,
    ) -> SelectOutcome {
        match deadline {
            Some(at) => tokio::select! {
                _ = fut => SelectOutcome::Completed,
                _ = tokio::time::sleep_until(at) => SelectOutcome::Deadline,
                _ = cancel.changed() => {
                    if *cancel.borrow() { SelectOutcome::Cancelled }
                    else { SelectOutcome::Completed }
                }
            },
            None => tokio::select! {
                _ = fut => SelectOutcome::Completed,
                _ = cancel.changed() => {
                    if *cancel.borrow() { SelectOutcome::Cancelled }
                    else { SelectOutcome::Completed }
                }
            },
        }
    }

    /// `min(init · 2^(attempt-1), max)` — attempt 1 = init, growing per failure.
    fn backoff_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(31);
        let scaled = self
            .config
            .backoff_init_ms
            .saturating_mul(1u64 << shift);
        scaled.min(self.config.backoff_max_ms)
    }

    /// One connect-and-stream cycle: connect (racing the hard deadline), send the
    /// single `PullRequest{ partition, since = retained W }`, then apply the
    /// scan-then-tail stream until the socket cuts. `deadline` is the bootstrap
    /// hard timer (X5); while bootstrap is outstanding the connect wait and the
    /// first-`Noop` wait race it, marking complete best-effort if it fires (the
    /// connection keeps streaming regardless — `*deadline` is set `None`).
    async fn run_once(
        &self,
        cancel: &mut watch::Receiver<bool>,
        deadline: &mut Option<tokio::time::Instant>,
    ) -> RunOutcome {
        // ---- Connecting ---- resolve fresh (D3) + connect, racing the hard
        // deadline so a hung connect to an unreachable/stalled peer still lets the
        // node go bootstrap-complete.
        // A spurious (non-cancelling) `cancel.changed()` wake re-enters the race
        // via `continue` — the deadline arm stays armed. The old shape re-awaited
        // `try_connect` OUTSIDE the select, so a hard timer expiring during that
        // window was missed and the node never went bootstrap-complete by timer.
        let conn = loop {
            match *deadline {
                Some(at) => tokio::select! {
                    c = self.try_connect() => break c,
                    _ = tokio::time::sleep_until(at) => {
                        // Hard timer tripped mid-connect: best-effort complete for
                        // readiness, then back off + retry (now warm → no hard timer).
                        self.mark_bootstrap_complete();
                        *deadline = None;
                        return RunOutcome::ConnectFailed;
                    }
                    _ = cancel.changed() => {
                        if *cancel.borrow() { return RunOutcome::Cancelled; }
                        continue;
                    }
                },
                None => tokio::select! {
                    c = self.try_connect() => break c,
                    _ = cancel.changed() => {
                        if *cancel.borrow() { return RunOutcome::Cancelled; }
                        continue;
                    }
                },
            }
        };
        let conn = match conn {
            Some(c) => c,
            None => return RunOutcome::ConnectFailed,
        };
        // Mark the peer reached so readiness no longer treats it as unreachable.
        if !self.status_tx.borrow().ever_connected {
            self.status_tx.send_modify(|s| s.ever_connected = true);
        }

        // One PullRequest opens this flow's stream from the retained W. `since ==
        // (0,0)` ⇒ the server store-scans this flow's keyspace (every frame
        // stamped `at = W = head@scan-start`) then tails from W on this SAME
        // socket; `since > (0,0)` ⇒ it tails directly. There is no second request.
        let since = self.status_tx.borrow().watermark;
        let req = Frame::PullRequest {
            proto_ver: PROTO_VER,
            caller: self.self_ordinal.clone(),
            partition: self.partition,
            since,
        };
        if conn.send(req).await.is_err() {
            return RunOutcome::Disconnected;
        }

        // A warm resume (W > (0,0)) tails directly — already past bootstrap, so
        // advance W and apply the Reverse rule from the first frame. A cold pull
        // starts pre-bootstrap: apply ungated, hold W until the first Noop.
        let mut bootstrapped = since != Watermark::new(0, 0);
        // Bodies imported during the pre-`Noop` bootstrap phase — the
        // re-hydration diagnostic (Reclaim flow only): a pass that re-stalls at
        // the same value across reconnects would signal truncation (now
        // structurally impossible, but the gauge keeps proving it).
        let mut applied_in_bootstrap = 0u64;

        loop {
            // While bootstrap is outstanding, race the first frame against the
            // hard deadline; once complete (or warm), race cancel only.
            let arm = if bootstrapped { None } else { *deadline };
            let frame = match arm {
                Some(at) => tokio::select! {
                    f = conn.recv() => f,
                    _ = tokio::time::sleep_until(at) => {
                        // First Noop is taking too long. Mark complete best-effort
                        // for READINESS, materialise whatever partial pre-seed has
                        // arrived (Reclaim flow), then KEEP streaming on the same
                        // socket — the real Noop will complete it for real. No
                        // apply-gate collision exists to force a disconnect now.
                        self.mark_bootstrap_complete();
                        *deadline = None;
                        if self.is_reclaim() {
                            self.metrics.bump_repl_bootstrap_stalled();
                            self.metrics.set_repl_bootstrap_last_applied(applied_in_bootstrap);
                            self.signal_reclaim_all();
                        }
                        continue;
                    }
                    _ = cancel.changed() => {
                        if *cancel.borrow() { return RunOutcome::Cancelled; }
                        continue;
                    }
                },
                None => tokio::select! {
                    f = conn.recv() => f,
                    _ = cancel.changed() => {
                        if *cancel.borrow() { return RunOutcome::Cancelled; }
                        continue;
                    }
                },
            };
            match frame {
                Some(Frame::Data {
                    at,
                    op,
                    partition,
                    call_ref,
                    call_gen,
                    call_bgen,
                    body_ttl_ms,
                    origin_now_ms,
                    indexes,
                    body,
                }) => {
                    // Apply under the `(p,b)` rule for this flow + phase. No
                    // watermark gate — `(p,b)` dominance is the only idempotency.
                    let mode = if self.is_reclaim() && bootstrapped {
                        ApplyMode::Reverse
                    } else {
                        ApplyMode::ForwardOrBootstrap
                    };
                    let applied = self
                        .apply_to_store(
                            op, partition, &call_ref, call_gen, call_bgen, body_ttl_ms,
                            origin_now_ms, &indexes, body, mode,
                        )
                        .await;
                    if !bootstrapped {
                        applied_in_bootstrap += 1;
                    }
                    // Pre-bootstrap frames share `at = W`; only the post-bootstrap
                    // tail advances W (and signals the per-call reclaim straggler).
                    if bootstrapped {
                        self.status_tx.send_modify(|s| s.watermark = at);
                        // Reclaim straggler: a backup reverse-flushed one of OUR
                        // calls after the bulk sweep — re-materialise + re-serve it.
                        // Only when the reverse-flush ACTUALLY applied: a Put the
                        // `(p,b)` gate dropped as dominated left the store untouched,
                        // so re-serving from it would be a spurious reclaim of a
                        // stale body (idempotent, but wasted work).
                        if self.is_reclaim() && op == Op::Put && applied {
                            if let Some(tx) = &self.repl_tx {
                                let _ = tx.send(ReplCommand::ReclaimCall(call_ref.clone()));
                            }
                        }
                    }
                }
                Some(Frame::Noop { at }) => {
                    let first = !bootstrapped;
                    self.status_tx.send_modify(|s| {
                        s.current = true;
                        if at > s.watermark {
                            s.watermark = at;
                        }
                    });
                    if first {
                        // First catch-up Noop ends the bootstrap: from here the
                        // tail advances W and (Reclaim) applies the Reverse rule.
                        bootstrapped = true;
                        self.mark_bootstrap_complete();
                        *deadline = None;
                        // Reclaim flow: bulk-materialise everything the bootstrap
                        // scan imported into the live serving map (smoothed).
                        if self.is_reclaim() {
                            self.metrics.bump_repl_bootstrap_seeded();
                            self.metrics.set_repl_bootstrap_last_applied(applied_in_bootstrap);
                            self.signal_reclaim_all();
                        }
                    }
                }
                Some(Frame::ResetToBootstrap { .. }) => {
                    // The server says our `since` fell off the compacted tail.
                    // Discard W AND clear bootstrap-complete so the next connect
                    // re-runs the full scan-then-tail. Bump `reset_gen` so the
                    // supervisor pulls its retained watermark DOWN too (a respawn
                    // must not resume from the now-invalid W).
                    self.status_tx.send_modify(|s| {
                        s.watermark = Watermark::new(0, 0);
                        s.bootstrap_complete = false;
                        s.reset_gen = s.reset_gen.saturating_add(1);
                    });
                    // RE-ARM the bootstrap hard deadline (X5): clearing
                    // `bootstrap_complete` without it left the forced re-bootstrap
                    // with NO liveness escape — if the peer then went unreachable,
                    // every retry was ConnectFailed with `deadline == None`, the
                    // best-effort timer could never fire, and `all_bootstrapped()`
                    // pinned the node NotReady until the dead peer returned. The
                    // re-bootstrap gets exactly the bound a cold boot gets.
                    *deadline = Some(
                        tokio::time::Instant::now()
                            + Duration::from_millis(self.config.bootstrap_hard_timeout_ms),
                    );
                    return RunOutcome::Disconnected;
                }
                // PullRequest is client→server; never expected here. Ignore.
                Some(_) => {}
                None => return RunOutcome::Disconnected,
            }
        }
    }

    /// Apply a `Data` frame's mutation to the local store under the **ADR-0014
    /// `(p,b)` version-vector rule**, **without** touching the watermark.
    ///
    /// `partition = Bak` → store as `bak:{primary}` (a **Forward** primary→backup
    /// update); `partition = Pri` → store as `pri:{primary}` (a **Reverse**
    /// backup→primary reverse-flush, or a bootstrap import).
    ///
    /// Apply decision (`(call_gen, call_bgen)` = incoming `(p,b)`; `(sp, sb)` =
    /// stored):
    /// - **Delete** → apply unconditionally (delete-wins, both directions).
    /// - [`ForwardOrBootstrap`](ApplyMode::ForwardOrBootstrap) `Put` → apply
    ///   unless the stored `(sp, sb)` strictly dominates (the follower / bootstrap
    ///   recovery defers to the authority but won't regress to a stale re-delivery).
    /// - [`Reverse`](ApplyMode::Reverse) `Put` → apply iff the primary has **not**
    ///   itself mutated past the backup's branch point and the backup genuinely
    ///   advanced: `p_in == sp && b_in > sb` (or no local copy yet → accept; the
    ///   reactive reclaim materialises it). Else keep our own.
    ///
    /// (Tombstone-suppress — never resurrect a locally-deleted call — is deferred:
    /// it needs a reaped tombstone set; deletes already win, so the remaining gap
    /// is only a late reverse-create racing a delete, which the watermark + reap
    /// bound.)
    /// Returns whether the mutation actually changed the store: a `Put` the
    /// `(p,b)` gate dropped as dominated returns `false`; an applied `Put` and any
    /// `Delete` (delete-wins) return `true`. The reclaim tail uses this to suppress
    /// a spurious `ReclaimCall` on a dropped reverse-flush.
    #[allow(clippy::too_many_arguments)]
    async fn apply_to_store(
        &self,
        op: Op,
        partition: Partition,
        call_ref: &str,
        call_gen: i64,
        call_bgen: i64,
        body_ttl_ms: i64,
        origin_now_ms: i64,
        indexes: &[String],
        body: Option<Arc<[u8]>>,
        mode: ApplyMode,
    ) -> bool {
        let primary = partition_of(&self.self_ordinal, call_ref).1;
        let role = match partition {
            Partition::Bak => PartitionRole::Backup,
            Partition::Pri => PartitionRole::Primary,
        };

        match op {
            Op::Put => {
                let stored = self.store.current_cv(role, &primary, call_ref);
                // Stored `(sp,sb)` DOMINATES incoming `(p,b)` ⇒ skip (idempotent /
                // reordered re-delivery). The dominance gate is now the ONLY
                // idempotency (no watermark apply-gate): it is what makes every
                // bootstrap frame — all sharing `at = W` — safe to apply.
                let dominated =
                    |sp: i64, sb: i64| sp >= call_gen && sb >= call_bgen;
                let apply = match mode {
                    // Bootstrap recovery / Forward primary→backup: authority's
                    // body, monotone by `(p,b)`.
                    ApplyMode::ForwardOrBootstrap => match stored {
                        Some((sp, sb)) => !dominated(sp, sb),
                        None => true,
                    },
                    // Reverse (backup → primary): accept iff untouched-by-us since
                    // the backup branched (`p_in == sp`) AND a genuinely newer
                    // backup mutation (`b_in > sb`); no local copy → accept (the
                    // reactive reclaim materialises it). Else keep our own.
                    ApplyMode::Reverse => match stored {
                        Some((sp, sb)) => call_gen == sp && call_bgen > sb,
                        None => true,
                    },
                };
                if apply {
                    let body = body.map(|b| b.to_vec()).unwrap_or_default();
                    let _ = self
                        .store
                        .put_call(
                            role,
                            &primary,
                            call_ref,
                            body,
                            indexes,
                            body_ttl_ms,
                            call_gen,
                            call_bgen,
                            // Apply locally only — do NOT re-propagate (peer:None).
                            // Carry the origin wall clock so the store can persist
                            // the receive-time skew offset for later timer
                            // re-anchoring on failover/reclaim (clock-skew hardening).
                            &PutOpts {
                                origin_now_ms: Some(origin_now_ms),
                                ..PutOpts::default()
                            },
                        )
                        .await;
                    // Inbound replica admitted — record the op per stream+endpoint:
                    // a brand-new ref (`stored.is_none()`) is a create, an existing
                    // one an update. The `recovery`/`create` step is the rebooted
                    // primary's bulk reclaim made visible; `backup`/`create` is
                    // steady forward replication landing.
                    let op = if stored.is_none() { "create" } else { "update" };
                    self.metrics.record_repl_applied(self.flow_label(), self.peer_ordinal(), op);
                }
                apply
            }
            Op::Delete => {
                let _ = self
                    .store
                    .delete_call(role, &primary, call_ref, indexes, &PutOpts::default())
                    .await;
                self.metrics.record_repl_applied(self.flow_label(), self.peer_ordinal(), "delete");
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::metrics::B2buaMetrics;
    use crate::repl::ReplicatingCallStore;
    use crate::store::{CallStore, PartitionRole};
    use async_trait::async_trait;
    use repl_net::frame::{Frame, Op, Partition, Watermark};
    use repl_net::transport::{
        ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
        SendError,
    };
    use sip_clock::Clock;
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::sync::Mutex;
    use tokio::sync::watch;

    /// A scripted client connection that delivers each pre-arranged bootstrap
    /// frame after a fixed clock-time `gap`, then parks (a quiet tail). Faithful
    /// to the real server, EVERY bootstrap `Data` carries the SAME scan-start
    /// head `at` — the collision the old apply-gate used to drop.
    struct PacedConn {
        frames: Mutex<VecDeque<Frame>>,
        gap_ms: u64,
    }

    #[async_trait]
    impl ReplicationConnection for PacedConn {
        async fn send(&self, _frame: Frame) -> Result<(), SendError> {
            Ok(())
        }
        async fn recv(&self) -> Option<Frame> {
            tokio::time::sleep(Duration::from_millis(self.gap_ms)).await;
            let next = self.frames.lock().unwrap().pop_front();
            match next {
                Some(f) => Some(f),
                // Script exhausted: the tail is quiet — park forever.
                None => std::future::pending().await,
            }
        }
        fn peer_addr(&self) -> SocketAddr {
            "127.0.0.1:9".parse().unwrap()
        }
        fn local_addr(&self) -> SocketAddr {
            "127.0.0.1:8".parse().unwrap()
        }
    }

    /// A network whose every `connect` hands back a fresh paced bootstrap script:
    /// `n` `Data{Pri,Put}` frames (all stamped the same `w_scan`) then the
    /// first catch-up `Noop{w_scan}`.
    struct PacedNet {
        n: usize,
        gap_ms: u64,
        w_scan: Watermark,
    }

    #[async_trait]
    impl ReplicationNetwork for PacedNet {
        async fn connect(
            &self,
            _dst: SocketAddr,
        ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
            let mut q = VecDeque::new();
            for i in 0..self.n {
                q.push_back(Frame::Data {
                    at: self.w_scan, // SAME head for every pre-seed frame
                    op: Op::Put,
                    partition: Partition::Pri,
                    call_ref: format!("w1|{i}|t"),
                    call_gen: 1,
                    call_bgen: 0,
                    body_ttl_ms: 0,
                    origin_now_ms: 0,
                    indexes: Vec::new(),
                    body: Some(Arc::from(format!("body{i}").into_bytes().into_boxed_slice())),
                });
            }
            q.push_back(Frame::Noop { at: self.w_scan });
            Ok(Box::new(PacedConn {
                frames: Mutex::new(q),
                gap_ms: self.gap_ms,
            }))
        }
        async fn listen(
            &self,
            _local: SocketAddr,
        ) -> Result<Box<dyn ReplicationListener>, ListenError> {
            unreachable!("the paced net is pull-only")
        }
    }

    /// Regression (#1, re-hydration truncation): a bootstrap whose `n` frames all
    /// share `at = W` must re-hydrate IN FULL. Pre-fix the client-side apply-gate
    /// (`at <= W → skip`) admitted only the first and dropped the rest (the
    /// ~203/3000 ceiling); now there is no gate — `(p,b)` dominance is the only
    /// idempotency, so every frame applies. The whole stream also outlasts the
    /// hard-timeout window, which (new model) no longer abandons the connection.
    #[tokio::test(start_paused = true)]
    async fn bootstrap_frames_sharing_watermark_all_rehydrate() {
        let clock = Clock::test_at(0);
        let n = 10usize;
        let store = ReplicatingCallStore::new(1, clock.clone());
        let net: Arc<dyn ReplicationNetwork> = Arc::new(PacedNet {
            n,
            gap_ms: 100,
            w_scan: Watermark::new(1, 100),
        });
        let config = PullerConfig {
            backoff_init_ms: 50,
            backoff_max_ms: 1_000,
            bootstrap_hard_timeout_ms: 500, // stream runs 1000ms > this, on purpose
        };
        let (puller, status) = Puller::new_at(
            "w0",
            "w1",
            Partition::Pri,
            "127.0.0.1:9".parse().unwrap(),
            net,
            store.clone(),
            config,
            Watermark::new(0, 0),
            B2buaMetrics::new(),
        );
        let (_cancel_tx, cancel_rx) = watch::channel(false);
        tokio::spawn(async move { puller.run(cancel_rx).await });

        // Drive the paused clock well past the 1000ms stream (+ tail settle).
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(100)).await;
            tokio::task::yield_now().await;
        }

        // ALL n pre-seed calls landed in pri:{w1} — none lost to an apply-gate
        // collision. Pre-fix only the first survived.
        for i in 0..n {
            let cr = format!("w1|{i}|t");
            assert!(
                store
                    .get_call(PartitionRole::Primary, "w1", &cr)
                    .await
                    .unwrap()
                    .is_some(),
                "call {i} must be re-hydrated into pri:{{w1}} (no client-side apply-gate to drop it)",
            );
        }
        assert!(
            status.borrow().bootstrap_complete,
            "first catch-up Noop observed ⇒ bootstrap-complete",
        );
    }
}
