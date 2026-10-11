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
//! compare. Every bootstrap frame shares `at = W`, so an `at <= W → skip` gate
//! would admit only the first and silently drop the rest.
//!
//! ## Watermark advances only on `Noop` and post-bootstrap tail `Data.at`
//! `W` is a pure **changelog position**, never the `(p,b)` vector. It advances on
//! a `Noop(head)` and on a post-bootstrap tail `Data.at` — **never** on a
//! bootstrap frame (they share `at = W`; advancing mid-scan then disconnecting
//! would skip the un-sent remainder). The per-run `bootstrapped` flag flips on
//! the first `Noop`; before it, frames apply ungated and do not move `W`.
//!
//! ## `ApplyMode = f(flow, bootstrapped)`
//! - **Backup** flow (`Bak`) — *Forward*, bootstrapped or not: apply unless
//!   dominated AND never behind the Element's own `b` (ADR-0031 D3). A bulk
//!   re-seed of `bak:{peer}` is a forward flush in bulk; the backup's progress
//!   is just as real there.
//! - **Reclaim** flow (`Pri`) — *Bootstrap* before the first `Noop` (recovering
//!   our own partition: take the replica's `(p,b)` as-is, ADR-0014 §3), then
//!   the *Reverse* rule after (`p_in == sp && b_in > sb`: a backup
//!   reverse-flushed one of our calls).
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
//! - after a post-bootstrap `Data` and on every `Noop` → `Position{at}` back to
//!   the server, the only client→server frame past the opening `PullRequest`
//!   (ADR-0031 D2).
//! - recv `None` / send `Err` / a send blocked `send_stall_ms` / nothing
//!   received for `idle_timeout_ms` → `Backoff` (RETAIN W).
//! - a connect not established within `connect_timeout_ms` → `Backoff`.
//! - `Backoff`: `sleep(min(init·2^attempt, max))` + a select on cancel (a plain
//!   `sleep`+`select`, **not** a `DelayQueue` — CLAUDE.md aliasing hazard). A
//!   stream that stayed connected for `idle_timeout_ms` resets `attempt`: the
//!   next wait is the floor.
//!
//! ## Link liveness (ADR-0045)
//! The server sends a `Noop` at least every few seconds, so a stream that
//! delivers nothing for `idle_timeout_ms` stands for a peer that went silent
//! without a reset; the puller leaves it and pulls again from W. Both bounds
//! hold in every phase, bootstrap or not.
//!
//! ## Bootstrap hard timer (ADR-0011 X5 — liveness over completeness)
//! A cold puller arms one absolute deadline `now + bootstrap_hard_timeout_ms`.
//! While bootstrap is outstanding the connect wait **and** the first-`Noop` wait
//! race it; if it fires the puller goes **bootstrap-complete (best-effort)** so
//! the node boots and serves even when a peer is unreachable or pathologically
//! slow. It does **not** abandon the connection: it keeps streaming on the same
//! socket and the real first `Noop` (when it arrives) completes the bootstrap for
//! real.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use call::MsgpackCodec;
use repl_net::frame::{Frame, Op, Partition, Watermark};
use repl_net::transport::{ReplicationConnection, ReplicationNetwork};
use tokio::sync::{mpsc, watch};
use topology::Peer;

use super::{AddrResolver, ReplicatingCallStore};
use crate::router::ReplCommand;

mod apply;
mod config;
use apply::{ApplyMode, ApplyOutcome};
pub use config::PullerConfig;

/// Protocol version stamped on the `PullRequest`. **v3** (ADR-0014 stream split):
/// the four-frame `{PullRequest, Data, Noop, ResetToBootstrap}` form (one
/// socket = one flow, scan-then-tail), `Op` = `{Put, Delete}`. A v3 node cannot
/// interpret a v2 peer's frames.
const PROTO_VER: u16 = 3;

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
    /// Sticky **bootstrap-complete** flag (ADR-0011 X5). Set when the first
    /// catch-up `Noop` arrives OR the bootstrap hard timer fires (best-effort) OR
    /// the puller resumes warm (`W > (0,0)` — no bootstrap needed). Cleared only
    /// by a `ResetToBootstrap`. Readiness consumes this.
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
    /// Connection cut or left after a successful connect → Backoff (retain
    /// W). `lasted`: the stream stayed connected for the idle bound, so the
    /// backoff starts again from its floor.
    Disconnected { lasted: bool },
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

/// Resolves once `cancel` reads `true` or its sender is gone: with no sender
/// left nothing can park the puller, so it stops. Cancel-safe, so a `select!`
/// arm can race it against the step in flight.
async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
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
    /// store directly).
    repl_tx: Option<mpsc::UnboundedSender<ReplCommand>>,
    /// The memory admission gate whose backup ceilings bound the replicas this
    /// node stores for its peers (ADR-0037). `None` stores every replica.
    capacity: Option<crate::capacity::CapacityGate>,
    /// Decoder for a stored Element's body. The Forward `Delete` guard reads the
    /// Element's lifecycle state, and the store persists the same encoding.
    codec: MsgpackCodec,
    /// When this puller's bootstrap started — the `duration_ms` on the
    /// bootstrap-complete lifecycle line. `None` for a warm resume (no
    /// bootstrap to time). Written from the single task that runs the FSM; the
    /// mutex is what keeps `&Puller` `Sync` across its awaits.
    bootstrap_started_at: std::sync::Mutex<Option<tokio::time::Instant>>,
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
                capacity: None,
                codec: MsgpackCodec::new(),
                bootstrap_started_at: std::sync::Mutex::new(
                    (!warm).then(tokio::time::Instant::now),
                ),
            },
            status_rx,
        )
    }

    /// Attach the router command sink (ADR-0011 X11 fail-back). A puller built
    /// without it (sim + unit tests, which assert on the store directly) sends no
    /// router commands.
    pub fn with_repl_sink(mut self, tx: mpsc::UnboundedSender<ReplCommand>) -> Self {
        self.repl_tx = Some(tx);
        self
    }

    /// The position this flow may report having applied, given it received
    /// up to `at`: a Backup flow never claims past a replica it left unstored
    /// (ADR-0037), so a draining primary does not read a shed call as held
    /// (ADR-0031 D2). A Reclaim flow sheds nothing.
    fn claimable(&self, at: Watermark) -> Watermark {
        if self.is_reclaim() {
            return at;
        }
        match self.store.shed_floor(self.peer_ordinal(), at.gen) {
            Some(floor) if floor < at => floor,
            _ => at,
        }
    }

    /// Attach the memory admission gate whose backup ceilings this puller
    /// applies (ADR-0037).
    pub fn with_capacity(mut self, gate: crate::capacity::CapacityGate) -> Self {
        self.capacity = Some(gate);
        self
    }

    /// Convenience constructor for a puller that always connects to a **fixed**
    /// `peer_addr` (a resolver-less caller / tests with a concrete bound socket).
    /// Wraps `peer_addr` in a constant [`FnPeerResolver`](super::FnPeerResolver).
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
        let resolve: AddrResolver = Arc::new(super::FnPeerResolver(move |_: &Peer| peer_addr));
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
        if self.is_reclaim() {
            "recovery"
        } else {
            "backup"
        }
    }

    /// One puller FSM transition, one line (ADR-0026): per `(peer, flow)` these
    /// are rare state changes, not per-call events.
    fn note_state(&self, state: &'static str, reason: &str) {
        tracing::info!(
            node = observe::node(),
            peer = %self.peer.ordinal,
            flow = self.flow_label(),
            state,
            reason,
            "puller state"
        );
    }

    /// Resolve the peer's address (fresh) and open a connection within
    /// `connect_timeout_ms`. `None` if the address is unresolvable right now or
    /// the connect fails or runs out of time — the caller treats each as a
    /// failed connect (→ Backoff, then retry/re-resolve).
    async fn try_connect(&self) -> Option<Box<dyn ReplicationConnection>> {
        let addr = self.resolve.resolve(&self.peer).await?;
        let bound = Duration::from_millis(self.config.connect_timeout_ms);
        match tokio::time::timeout(bound, self.network.connect(addr)).await {
            Ok(conn) => conn.ok(),
            Err(_) => {
                tracing::debug!(peer = %self.peer.ordinal, %addr, "repl connect timed out");
                None
            }
        }
    }

    /// Run the FSM until `cancel` flips to `true` (Park / shutdown) or its sender
    /// is dropped. `cancel` is a `watch` so the supervisor can interrupt a
    /// parked-out puller atomically.
    pub async fn run(self, mut cancel: watch::Receiver<bool>) {
        let mut attempt: u32 = 0;
        // Arm the bootstrap hard deadline iff we start cold. A warm resume is
        // already `bootstrap_complete` (set in `new`), so no deadline is needed.
        let mut deadline: Option<tokio::time::Instant> =
            if self.status_tx.borrow().bootstrap_complete {
                self.note_state("warm", "resumed from a retained watermark");
                None
            } else {
                self.note_state("bootstrap_start", "cold watermark");
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
                outcome @ (RunOutcome::ConnectFailed | RunOutcome::Disconnected { .. }) => {
                    // ---- Backoff{attempt} ---- retain W (status already holds it).
                    if outcome == (RunOutcome::Disconnected { lasted: true }) {
                        attempt = 0;
                    }
                    attempt = attempt.saturating_add(1);
                    let ms = self.backoff_ms(attempt);
                    self.note_state("backoff", &format!("attempt {attempt}, retry in {ms}ms"));
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
                            self.mark_bootstrap_complete(0, "hard timeout while backing off");
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
    /// catch-up `Noop` and on the hard-timer firing (best-effort). Lines the
    /// per-peer completion once — `applied` bodies imported, `reason` telling a
    /// real catch-up from a best-effort timer expiry.
    fn mark_bootstrap_complete(&self, applied: u64, reason: &'static str) {
        if !self.status_tx.borrow().bootstrap_complete {
            let duration_ms = self
                .bootstrap_started_at
                .lock()
                .unwrap()
                .take()
                .map(|at| at.elapsed().as_millis() as u64)
                .unwrap_or(0);
            tracing::info!(
                node = observe::node(),
                peer = %self.peer.ordinal,
                flow = self.flow_label(),
                applied,
                duration_ms,
                reason,
                "peer bootstrap complete"
            );
        }
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
                () = cancelled(cancel) => SelectOutcome::Cancelled,
            },
            None => tokio::select! {
                _ = fut => SelectOutcome::Completed,
                () = cancelled(cancel) => SelectOutcome::Cancelled,
            },
        }
    }

    /// Leave a stream that delivered nothing for `idle_timeout_ms`: the peer
    /// went silent without a reset (ADR-0045).
    fn left_idle(&self, connected_at: tokio::time::Instant) -> RunOutcome {
        self.note_state(
            "idle",
            &format!("nothing received for {}ms, pulling again", self.config.idle_timeout_ms),
        );
        self.ended(connected_at)
    }

    /// A stream connected at `connected_at` ended.
    fn ended(&self, connected_at: tokio::time::Instant) -> RunOutcome {
        let idle = Duration::from_millis(self.config.idle_timeout_ms);
        RunOutcome::Disconnected { lasted: connected_at.elapsed() >= idle }
    }

    /// Send `frame` within `send_stall_ms`; `false` when the stream failed or
    /// the send stayed blocked (ADR-0045).
    async fn send(&self, conn: &dyn ReplicationConnection, frame: Frame) -> bool {
        let bound = Duration::from_millis(self.config.send_stall_ms);
        match tokio::time::timeout(bound, conn.send(frame)).await {
            Ok(sent) => sent.is_ok(),
            Err(_) => {
                self.note_state("stalled", "a send stayed blocked, pulling again");
                false
            }
        }
    }

    /// `min(init · 2^(attempt-1), max)` — attempt 1 = init, growing per failure.
    fn backoff_ms(&self, attempt: u32) -> u64 {
        let shift = attempt.saturating_sub(1).min(31);
        let scaled = self.config.backoff_init_ms.saturating_mul(1u64 << shift);
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
        let conn = match *deadline {
            Some(at) => tokio::select! {
                c = self.try_connect() => c,
                _ = tokio::time::sleep_until(at) => {
                    // Hard timer tripped mid-connect: best-effort complete for
                    // readiness, then back off + retry (now warm → no hard timer).
                    self.mark_bootstrap_complete(0, "hard timeout while connecting");
                    *deadline = None;
                    return RunOutcome::ConnectFailed;
                }
                () = cancelled(cancel) => return RunOutcome::Cancelled,
            },
            None => tokio::select! {
                c = self.try_connect() => c,
                () = cancelled(cancel) => return RunOutcome::Cancelled,
            },
        };
        let conn = match conn {
            Some(c) => c,
            None => return RunOutcome::ConnectFailed,
        };
        // Mark the peer reached so readiness no longer treats it as unreachable.
        if !self.status_tx.borrow().ever_connected {
            self.status_tx.send_modify(|s| s.ever_connected = true);
        }
        self.note_state("streaming", "connected");
        let connected_at = tokio::time::Instant::now();

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
        if !self.send(conn.as_ref(), req).await {
            return self.ended(connected_at);
        }

        // A warm resume (W > (0,0)) tails directly — already past bootstrap, so
        // advance W and apply the Reverse rule from the first frame. A cold pull
        // starts pre-bootstrap: apply ungated, hold W until the first Noop.
        let mut bootstrapped = since != Watermark::new(0, 0);
        // A Backup scan re-sends every live body of the primary, so the shed
        // marks of an earlier stream (an old incarnation, a ref deleted while
        // disconnected) say nothing about it: they go, and a body still left
        // out marks itself again (ADR-0037).
        if !bootstrapped && !self.is_reclaim() {
            self.store.clear_shed_of(self.peer_ordinal());
        }
        // Bodies imported during the pre-`Noop` bootstrap phase — the
        // re-hydration diagnostic (Reclaim flow only): a pass that re-stalls at
        // the same value across reconnects would signal truncation
        // (structurally impossible, but the gauge keeps proving it).
        let mut applied_in_bootstrap = 0u64;
        // The idle bound (ADR-0045): a stream that delivers nothing for this long
        // is left, whatever the phase.
        let idle = Duration::from_millis(self.config.idle_timeout_ms);
        let mut quiet_until = tokio::time::Instant::now() + idle;

        loop {
            // While bootstrap is outstanding, race the first frame against the
            // hard deadline; once complete (or warm), race cancel only. Both
            // race the idle bound.
            let arm = if bootstrapped { None } else { *deadline };
            let frame = match arm {
                Some(at) => tokio::select! {
                    f = conn.recv() => f,
                    _ = tokio::time::sleep_until(quiet_until) => {
                        return self.left_idle(connected_at);
                    }
                    _ = tokio::time::sleep_until(at) => {
                        // First Noop is taking too long. Mark complete best-effort
                        // for READINESS, materialise whatever partial pre-seed has
                        // arrived (Reclaim flow), then KEEP streaming on the same
                        // socket — the real Noop will complete it for real. No
                        // apply-gate collision exists to force a disconnect.
                        self.mark_bootstrap_complete(
                            applied_in_bootstrap,
                            "hard timeout awaiting the first catch-up Noop",
                        );
                        *deadline = None;
                        if self.is_reclaim() {
                            self.metrics.bump_repl_bootstrap_stalled();
                            self.metrics.set_repl_bootstrap_last_applied(applied_in_bootstrap);
                            self.signal_reclaim_all();
                        }
                        continue;
                    }
                    () = cancelled(cancel) => return RunOutcome::Cancelled,
                },
                None => tokio::select! {
                    f = conn.recv() => f,
                    _ = tokio::time::sleep_until(quiet_until) => {
                        return self.left_idle(connected_at);
                    }
                    () = cancelled(cancel) => return RunOutcome::Cancelled,
                },
            };
            if frame.is_some() {
                quiet_until = tokio::time::Instant::now() + idle;
            }
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
                    answered,
                    incarnation,
                }) => {
                    // Apply under the `(p,b)` rule for this flow + phase. No
                    // watermark gate — `(p,b)` dominance is the only idempotency.
                    let mode = match (self.is_reclaim(), bootstrapped) {
                        (true, true) => ApplyMode::Reverse,
                        (true, false) => ApplyMode::Bootstrap,
                        (false, _) => ApplyMode::Forward,
                    };
                    let refused_body = body.clone();
                    let outcome = self
                        .apply_to_store(
                            op,
                            partition,
                            &call_ref,
                            call_gen,
                            call_bgen,
                            body_ttl_ms,
                            origin_now_ms,
                            &indexes,
                            body,
                            answered,
                            incarnation,
                            mode,
                        )
                        .await;
                    if outcome == ApplyOutcome::Shed {
                        let floor = self.status_tx.borrow().watermark;
                        self.store.note_shed(
                            &call_ref,
                            self.peer_ordinal(),
                            at.gen,
                            floor,
                            body_ttl_ms,
                        );
                    }
                    if !bootstrapped {
                        applied_in_bootstrap += 1;
                    }
                    if op == Op::Put {
                        self.signal_put_outcome(
                            mode,
                            outcome,
                            bootstrapped,
                            &call_ref,
                            refused_body,
                            origin_now_ms,
                        );
                    }
                    // Pre-bootstrap frames share `at = W`; only the post-bootstrap
                    // tail advances W.
                    if bootstrapped {
                        self.status_tx.send_modify(|s| s.watermark = at);
                        // Report the applied position so a withdrawn peer's drain
                        // can see its calls held here (ADR-0031 D2). Pre-bootstrap
                        // frames all share `at = W`: claiming W before the whole
                        // scan is applied would be false, so the bootstrap claim
                        // is made once, on the first catch-up `Noop`.
                        if !self
                            .send(conn.as_ref(), Frame::Position { at: self.claimable(at) })
                            .await
                        {
                            return self.ended(connected_at);
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
                        self.mark_bootstrap_complete(applied_in_bootstrap, "first catch-up Noop");
                        *deadline = None;
                        // Reclaim flow: bulk-materialise everything the bootstrap
                        // scan imported into the live serving map (smoothed).
                        if self.is_reclaim() {
                            self.metrics.bump_repl_bootstrap_seeded();
                            self.metrics.set_repl_bootstrap_last_applied(applied_in_bootstrap);
                            self.signal_reclaim_all();
                        }
                    }
                    // Every `Noop` is answered with the position it announced:
                    // the first one is the bootstrap's own claim (every scan body
                    // is applied by now), the idle ones keep the claim fresh
                    // (ADR-0031 D2).
                    if !self.send(conn.as_ref(), Frame::Position { at: self.claimable(at) }).await {
                        return self.ended(connected_at);
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
                    // RE-ARM the bootstrap hard deadline (X5): without it the forced
                    // re-bootstrap has NO liveness escape — an unreachable peer
                    // fails every retry with `deadline == None`, and
                    // `all_bootstrapped()` pins the node NotReady until it returns.
                    // The re-bootstrap gets exactly the bound a cold boot gets.
                    *deadline = Some(
                        tokio::time::Instant::now()
                            + Duration::from_millis(self.config.bootstrap_hard_timeout_ms),
                    );
                    *self.bootstrap_started_at.lock().unwrap() = Some(tokio::time::Instant::now());
                    self.note_state("reset_to_bootstrap", "server dropped our watermark");
                    return self.ended(connected_at);
                }
                // PullRequest is client→server; never expected here. Ignore.
                Some(_) => {}
                None => return self.ended(connected_at),
            }
        }
    }
}

#[cfg(test)]
mod tests;
