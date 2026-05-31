//! [`Puller`] — the per-peer replication **client FSM** (migration slice S5).
//!
//! One `Puller` drives the client side for ONE peer ordinal: it opens a
//! connection, issues a single `PullRequest`, and applies the long-lived stream
//! of `Data` / `Noop` the server pushes, reconnecting with exponential backoff
//! across cuts. The watermark `W` it resumes from is **owned by the supervisor**
//! (retained per-ordinal across Park/disconnect/re-add), passed in at each run.
//!
//! ## FSM (the plan's table)
//! ```text
//! States: Parked · Connecting · Bootstrapping · Tailing{ever_current} · Backoff{attempt}
//! ```
//! - start / topology-add → `Connecting` (`network.connect`).
//! - connect ok, cold-start/reset → `Bootstrapping`:
//!   `PullRequest(Bootstrap, since=(0,0))` → apply the lazy-batch `Data(Pri)`
//!   pre-seed (import as `pri:{me}`, LWW by `call_gen`, NOT watermark-gated) →
//!   on the terminal `Noop` mark **bootstrap-complete** → re-pull
//!   `PullRequest(Replog, since=(0,0))` on the SAME connection. The Replog
//!   re-pull is intentionally **cold** so the compacted changelog delivers the
//!   full mutated set across both partitions (the static `bak:{me}` pre-seed +
//!   the cold changelog walk together re-hydrate the node); its trailing
//!   `Noop(head)` advances `W`.
//! - connect ok, self has state (`W > (0,0)`) → `Tailing`,
//!   `PullRequest(Replog, since=retained W)`.
//! - connect fails → `Backoff{attempt++}`.
//! - `Tailing` `Data{at}`: apply iff `at > W` (apply-gate); LWW on `call_gen`;
//!   then `W = at`.
//! - `Tailing` `Noop{at}`: `ever_current = true` (sticky); advance `W` if `at`
//!   greater.
//! - `Tailing` `ResetToBootstrap` → `Bootstrapping` (discard W; re-pull — S5
//!   re-pulls from `(0,0)`).
//! - recv `None` / send `Err` → `Backoff` (RETAIN W).
//! - `Backoff`: `sleep(min(init·2^attempt, max))` + a select on the cancel
//!   signal → `Connecting`. A plain `sleep`+`select`, **not** a `DelayQueue`
//!   (CLAUDE.md aliasing hazard).
//!
//! A cold puller (after reboot/crash, empty store) converges by pulling from
//! `(0,0)`: the compacted changelog delivers the full live set.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Frame, Op, PullMode, Watermark};
use repl_net::transport::ReplicationNetwork;
use tokio::sync::watch;

use super::ReplicatingCallStore;
use crate::store::{partition_of, CallStore, PartitionRole, PutOpts};

/// Default backoff floor / ceiling (ms). Exposed via [`PullerConfig`] for tests.
const DEFAULT_BACKOFF_INIT_MS: u64 = 100;
const DEFAULT_BACKOFF_MAX_MS: u64 = 30_000;
/// Default bootstrap hard-timeout (ms): the upper bound on how long the puller
/// waits for the terminal `Noop` before declaring bootstrap-complete best-effort
/// (X5 / Decision 4 — liveness over completeness).
const DEFAULT_BOOTSTRAP_HARD_TIMEOUT_MS: u64 = 10_000;
/// Protocol version stamped on the `PullRequest`.
const PROTO_VER: u16 = 1;
/// Server batch-size hint on the `PullRequest`.
const CHUNK: u32 = 128;

/// Backoff knobs for a puller (tests inject short values).
#[derive(Clone, Copy, Debug)]
pub struct PullerConfig {
    /// Backoff floor in ms (`init` in `min(init·2^attempt, max)`).
    pub backoff_init_ms: u64,
    /// Backoff ceiling in ms.
    pub backoff_max_ms: u64,
    /// Bootstrap hard-timeout in ms (X5): if the terminal `Noop` does not arrive
    /// within this budget — peer slow / stalled / unreachable — the puller stops
    /// waiting on bootstrap and marks **bootstrap-complete (best-effort)**,
    /// proceeding to `Tailing`/`Backoff`. The node boots and serves regardless.
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

/// The puller's FSM state. `Parked` is owned by the supervisor (it simply does
/// not run a puller); the running puller cycles
/// `Connecting → {Bootstrapping|Tailing} → Backoff → Connecting`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PullerState {
    /// Interrupted by topology `Removed`; not running (W retained by supervisor).
    Parked,
    /// Opening a connection to the peer.
    Connecting,
    /// Cold-start/reset re-hydration: request `PullMode::Bootstrap`, apply the
    /// lazy-batch `Data(Pri)` pre-seed (import as `pri:{me}`), seed `W` from the
    /// terminal `Noop`, then re-pull `Replog(since=W)` on the same connection.
    /// Bounded by the hard timer (X5) — best-effort complete on expiry.
    Bootstrapping,
    /// Steady-state tail. `ever_current` is the sticky current flag.
    Tailing { ever_current: bool },
    /// Backing off before the next connect attempt.
    Backoff { attempt: u32 },
}

/// Shared, observable per-peer puller status. The supervisor reads `current`
/// for readiness (`is_current` / `all_current`) and the retained `watermark`.
#[derive(Clone, Copy, Debug)]
pub struct PullerStatus {
    /// Sticky current flag — set on the first `Noop`, never cleared.
    pub current: bool,
    /// The retained watermark (advances on apply/Noop). The supervisor owns the
    /// authoritative copy keyed by ordinal; the puller publishes its progress
    /// here so a Park/re-add resumes from it.
    pub watermark: Watermark,
    /// Sticky **bootstrap-complete** flag (X5 / Decision 4). Set when the
    /// terminal bootstrap `Noop` arrives OR the bootstrap hard timer fires
    /// (best-effort) OR the puller resumes warm (`W > (0,0)` — no bootstrap
    /// needed). Cleared only by a `ResetToBootstrap`. S7 readiness consumes this.
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

/// Drives the client side for ONE peer. Cheap to construct; `run` loops until
/// cancelled. The watermark + current flag are published on a `watch` channel
/// the supervisor reads.
pub struct Puller {
    /// The peer ordinal this puller replicates from.
    peer_ordinal: String,
    /// This node's ordinal (apply-target keyspace resolution).
    self_ordinal: String,
    /// Peer replication address (re-resolved by the supervisor on AddressChanged).
    peer_addr: SocketAddr,
    network: Arc<dyn ReplicationNetwork>,
    /// Apply target — the local store.
    store: ReplicatingCallStore,
    config: PullerConfig,
    /// Published status (current flag + watermark) the supervisor observes.
    status_tx: watch::Sender<PullerStatus>,
}

impl Puller {
    /// Build a puller for `peer_ordinal` at `peer_addr`, seeded from `start_w`
    /// (the supervisor's retained watermark — `(0,0)` for a cold/never-seen
    /// peer). Returns the puller plus a [`watch::Receiver`] the supervisor reads.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        peer_ordinal: impl Into<String>,
        self_ordinal: impl Into<String>,
        peer_addr: SocketAddr,
        network: Arc<dyn ReplicationNetwork>,
        store: ReplicatingCallStore,
        config: PullerConfig,
        start_w: Watermark,
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
                peer_ordinal: peer_ordinal.into(),
                self_ordinal: self_ordinal.into(),
                peer_addr,
                network,
                store,
                config,
                status_tx,
            },
            status_rx,
        )
    }

    /// The peer ordinal this puller serves.
    pub fn peer_ordinal(&self) -> &str {
        &self.peer_ordinal
    }

    /// Run the FSM until `cancel` flips to `true` (Park / shutdown). `cancel` is
    /// a `watch` so the supervisor can interrupt a parked-out puller atomically.
    ///
    /// ## Bootstrap hard timer (X5 / Decision 4)
    /// If the puller starts cold (`bootstrap_complete == false`) it arms a single
    /// absolute deadline `now + bootstrap_hard_timeout_ms`. While bootstrap is
    /// outstanding, *every* blocking wait — connect, the bootstrap recv loop, and
    /// the backoff between failed connects — races this deadline. When it fires
    /// the puller marks **bootstrap-complete (best-effort)** and proceeds to
    /// `Tailing`/`Backoff` without ever blocking startup. A node whose peers are
    /// all unreachable still becomes complete once the deadline trips.
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
            // ---- Connecting / Bootstrapping / Tailing ----
            match self.run_once(&mut cancel, deadline).await {
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

    /// Mark this puller **bootstrap-complete** (sticky). Called on the terminal
    /// bootstrap `Noop` and on the hard-timer firing (best-effort).
    fn mark_bootstrap_complete(&self) {
        self.status_tx.send_modify(|s| s.bootstrap_complete = true);
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

    /// One connect-and-tail cycle. On a cold start it runs the **Bootstrapping**
    /// phase first (apply the pre-seed `Data(Pri)`, seed `W` from the terminal
    /// `Noop`, then re-pull `Replog` on the SAME connection) before falling into
    /// the steady-state tail. `deadline` is the bootstrap hard timer (X5): while
    /// bootstrap is outstanding the recv loop races it, marking complete + tailing
    /// best-effort if it fires.
    async fn run_once(
        &self,
        cancel: &mut watch::Receiver<bool>,
        deadline: Option<tokio::time::Instant>,
    ) -> RunOutcome {
        // ---- Connecting ---- race the hard deadline so a hung connect to an
        // unreachable/stalled peer still lets the node go bootstrap-complete.
        let conn = match deadline {
            Some(at) => tokio::select! {
                r = self.network.connect(self.peer_addr) => r,
                _ = tokio::time::sleep_until(at) => {
                    // Hard timer tripped mid-connect: best-effort complete, then
                    // back off + retry (now warm → no further hard timer).
                    self.mark_bootstrap_complete();
                    return RunOutcome::ConnectFailed;
                }
                _ = cancel.changed() => {
                    if *cancel.borrow() { return RunOutcome::Cancelled; }
                    self.network.connect(self.peer_addr).await
                }
            },
            None => tokio::select! {
                r = self.network.connect(self.peer_addr) => r,
                _ = cancel.changed() => {
                    if *cancel.borrow() { return RunOutcome::Cancelled; }
                    self.network.connect(self.peer_addr).await
                }
            },
        };
        let conn = match conn {
            Ok(c) => c,
            Err(_) => return RunOutcome::ConnectFailed,
        };
        // Mark the peer reached so readiness no longer treats it as unreachable.
        if !self.status_tx.borrow().ever_connected {
            self.status_tx.send_modify(|s| s.ever_connected = true);
        }

        // Decide cold-start (Bootstrapping) vs. resume (Tailing) from retained W.
        // Cold iff we have never advanced past `(0,0)` — i.e. never received a
        // terminal `Noop` or any tail data from this peer. We deliberately do NOT
        // also gate on `bootstrap_complete`: the bootstrap hard timer may set it
        // for an unreachable peer WITHOUT a real pre-seed (W still `(0,0)`), and
        // suppressing the bootstrap on the eventual real connect would skip the
        // static `bak:{me}` keyset scan (which the cold-Replog walk does not
        // cover) and silently leave the node missing those backups. Re-running
        // bootstrap when already current is at worst a redundant, idempotent pass.
        let w = self.status_tx.borrow().watermark;
        let cold = w == Watermark::new(0, 0);

        let since = if cold {
            // ---- Bootstrapping ---- request the lazy-batch pre-seed; the server
            // streams Data(Pri) (the static `bak:{me}` calls the peer holds but
            // never mutated — NOT in the changelog) then a TERMINAL Noop(W). We
            // apply the pre-seed directly (NOT watermark-gated, LWW by call_gen),
            // then re-pull `Replog` from `(0,0)` on this SAME connection.
            //
            // The re-pull is **cold** (`since.gen < server.gen`) on purpose: the
            // compacted changelog-for-me delivers the full mutated set across
            // BOTH partitions (`bak` = the peer's calls I back up; `pri` = my
            // calls it reverse-mutated). Bootstrap + cold-Replog are the one
            // re-hydration stream (X4). We must NOT seed W to the Noop head first
            // — that would gate the cold pull's `at <= head` entries out; instead
            // the cold pull applies from `W=(0,0)` and its trailing `Noop(head)`
            // advances W. Overlap between the pre-seed and the cold pull is
            // idempotent by call_gen.
            let req = Frame::PullRequest {
                proto_ver: PROTO_VER,
                caller: self.self_ordinal.clone(),
                mode: PullMode::Bootstrap,
                since: Watermark::new(0, 0),
                chunk: CHUNK,
            };
            if conn.send(req).await.is_err() {
                return RunOutcome::Disconnected;
            }
            match self.run_bootstrap(conn.as_ref(), cancel, deadline).await {
                BootstrapOutcome::Seeded(_w) => {
                    // Terminal Noop arrived (carrying the scan-start head `_w`) →
                    // bootstrap-complete. We deliberately leave W at (0,0) rather
                    // than seeding it to `_w`: the follow-up Replog pull must be
                    // **cold** (`since.gen < server.gen`) to re-deliver the full
                    // compacted set (both partitions). Seeding W=`_w` would gate
                    // out the cold pull's `at <= _w` entries. The cold pull's
                    // trailing Noop(head) advances W; overlap is idempotent.
                    self.mark_bootstrap_complete();
                }
                BootstrapOutcome::HardTimeout => {
                    // Best-effort complete; still cold-pull what we can.
                    self.mark_bootstrap_complete();
                }
                BootstrapOutcome::Cancelled => return RunOutcome::Cancelled,
                BootstrapOutcome::Disconnected => return RunOutcome::Disconnected,
            }
            // Cold Replog re-pull from (0,0) on the same connection.
            Watermark::new(0, 0)
        } else {
            // ---- Tailing (resume from retained W) ----
            self.status_tx.borrow().watermark
        };

        // Open (or re-open, after bootstrap) the steady-state Replog tail.
        let req = Frame::PullRequest {
            proto_ver: PROTO_VER,
            caller: self.self_ordinal.clone(),
            mode: PullMode::Replog,
            since,
            chunk: CHUNK,
        };
        if conn.send(req).await.is_err() {
            return RunOutcome::Disconnected;
        }

        // ---- Tailing loop ---- apply Data, set current on Noop, until cut.
        loop {
            let frame = tokio::select! {
                f = conn.recv() => f,
                _ = cancel.changed() => {
                    if *cancel.borrow() { return RunOutcome::Cancelled; }
                    continue;
                }
            };
            match frame {
                Some(Frame::Data {
                    at,
                    op,
                    partition,
                    call_ref,
                    call_gen,
                    body_ttl_ms,
                    indexes,
                    body,
                }) => {
                    self.apply_data(
                        at, op, partition, &call_ref, call_gen, body_ttl_ms, &indexes, body,
                    )
                    .await;
                }
                Some(Frame::Noop { at }) => {
                    // Sticky current; advance W if greater.
                    self.status_tx.send_modify(|s| {
                        s.current = true;
                        if at > s.watermark {
                            s.watermark = at;
                        }
                    });
                }
                Some(Frame::ResetToBootstrap { .. }) => {
                    // → Bootstrapping: the server says our `since` fell off the
                    // compacted tail. Discard W AND clear bootstrap-complete so
                    // the next connect re-runs the full lazy-batch pre-seed. Bump
                    // `reset_gen` so the supervisor pulls its retained watermark
                    // DOWN too (a respawn must not resume from the now-invalid W).
                    self.status_tx.send_modify(|s| {
                        s.watermark = Watermark::new(0, 0);
                        s.bootstrap_complete = false;
                        s.reset_gen = s.reset_gen.saturating_add(1);
                    });
                    return RunOutcome::Disconnected;
                }
                // PullRequest/Ack are client→server; never expected here. Ignore.
                Some(_) => {}
                None => return RunOutcome::Disconnected,
            }
        }
    }

    /// The **Bootstrapping** recv loop. Applies each pre-seed `Data` (partition
    /// `Pri` → import as `pri:{primary}`, LWW-guarded by `call_gen`, NOT
    /// watermark-gated since it is a bulk pre-seed) and returns on the TERMINAL
    /// `Noop{at}` ([`BootstrapOutcome::Seeded`]). Races the cancel signal and the
    /// bootstrap hard `deadline` — the latter yields
    /// [`BootstrapOutcome::HardTimeout`] (best-effort completion).
    async fn run_bootstrap(
        &self,
        conn: &dyn repl_net::transport::ReplicationConnection,
        cancel: &mut watch::Receiver<bool>,
        deadline: Option<tokio::time::Instant>,
    ) -> BootstrapOutcome {
        loop {
            let frame = match deadline {
                Some(at) => tokio::select! {
                    f = conn.recv() => f,
                    _ = tokio::time::sleep_until(at) => return BootstrapOutcome::HardTimeout,
                    _ = cancel.changed() => {
                        if *cancel.borrow() { return BootstrapOutcome::Cancelled; }
                        continue;
                    }
                },
                None => tokio::select! {
                    f = conn.recv() => f,
                    _ = cancel.changed() => {
                        if *cancel.borrow() { return BootstrapOutcome::Cancelled; }
                        continue;
                    }
                },
            };
            match frame {
                Some(Frame::Data {
                    op,
                    partition,
                    call_ref,
                    call_gen,
                    body_ttl_ms,
                    indexes,
                    body,
                    ..
                }) => {
                    // Pre-seed: apply directly (no watermark gate), LWW by call_gen.
                    self.apply_to_store(
                        op, partition, &call_ref, call_gen, body_ttl_ms, &indexes, body,
                    )
                    .await;
                }
                // TERMINAL: end of bootstrap; `at` is the scan-start head → seed W.
                Some(Frame::Noop { at }) => return BootstrapOutcome::Seeded(at),
                // A reset mid-bootstrap: bail to reconnect (will re-bootstrap).
                Some(Frame::ResetToBootstrap { .. }) => return BootstrapOutcome::Disconnected,
                Some(_) => {}
                None => return BootstrapOutcome::Disconnected,
            }
        }
    }

    /// Apply one `Data` frame under the apply-gate + LWW rules, then advance W
    /// (the steady-state tail path).
    #[allow(clippy::too_many_arguments)]
    async fn apply_data(
        &self,
        at: Watermark,
        op: Op,
        partition: repl_net::frame::Partition,
        call_ref: &str,
        call_gen: i64,
        body_ttl_ms: i64,
        indexes: &[String],
        body: Option<Arc<[u8]>>,
    ) {
        // ---- apply-gate ---- only entries strictly above W mutate.
        if at <= self.status_tx.borrow().watermark {
            return;
        }
        self.apply_to_store(op, partition, call_ref, call_gen, body_ttl_ms, indexes, body)
            .await;
        // Advance W past the applied entry.
        self.status_tx.send_modify(|s| s.watermark = at);
    }

    /// Apply a `Data` frame's mutation to the local store under the LWW guard,
    /// **without** touching the watermark. Shared by the tail apply-gate path
    /// ([`apply_data`](Self::apply_data)) and the bootstrap pre-seed path
    /// ([`run_bootstrap`](Self::run_bootstrap)).
    ///
    /// `partition=Bak` → store as `bak:{primary}`; `partition=Pri` → store as
    /// `pri:{primary}` (the reclaim/reverse keyspace — bootstrap imports land
    /// here). LWW: a stored `call_gen >= the frame's` skips the body write so a
    /// concurrent newer mutation is not clobbered by an older bootstrap copy.
    #[allow(clippy::too_many_arguments)]
    async fn apply_to_store(
        &self,
        op: Op,
        partition: repl_net::frame::Partition,
        call_ref: &str,
        call_gen: i64,
        body_ttl_ms: i64,
        indexes: &[String],
        body: Option<Arc<[u8]>>,
    ) {
        let primary = partition_of(&self.self_ordinal, call_ref).1;
        let role = match partition {
            repl_net::frame::Partition::Bak => PartitionRole::Backup,
            repl_net::frame::Partition::Pri => PartitionRole::Primary,
        };

        match op {
            Op::Create | Op::Update => {
                let stored = self.store.current_call_gen(role, &primary, call_ref);
                let skip = matches!(stored, Some(g) if g >= call_gen);
                if !skip {
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
                            // Apply locally only — do NOT re-propagate (peer:None).
                            &PutOpts::default(),
                        )
                        .await;
                }
            }
            Op::Delete => {
                let _ = self
                    .store
                    .delete_call(role, &primary, call_ref, indexes, &PutOpts::default())
                    .await;
            }
        }
    }
}

/// How the [`Bootstrapping`](PullerState::Bootstrapping) phase ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BootstrapOutcome {
    /// Terminal `Noop` arrived; seed W to this watermark and tail.
    Seeded(Watermark),
    /// The bootstrap hard timer fired — best-effort complete, tail from W.
    HardTimeout,
    /// Cancel signal fired (Park / shutdown).
    Cancelled,
    /// Connection cut / reset mid-bootstrap → reconnect (re-bootstrap).
    Disconnected,
}
