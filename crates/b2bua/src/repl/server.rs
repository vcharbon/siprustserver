//! [`ReplServer`] — the replication **serve-loop** (migration slice S5).
//!
//! A node `listen`s on its replication address and serves each inbound
//! connection a long-lived stream of changelog [`Frame::Data`] for the caller
//! that opened it. The protocol is the steady-state Exchange from the plan:
//!
//! ```text
//! peer → us:  PullRequest(Replog, since=W)                 # ONCE, opens a subscription
//! us → peer:  Data…(entries > W, ascending) ; Noop(head)   # drain, then caught-up marker
//!             …park on the changelog's subscriber Notify; on a new bump, drain again…
//! ```
//!
//! One `PullRequest` opens a **long-lived** subscription: the server PUSHES new
//! `Data` as the changelog grows (it never waits for another `PullRequest`).
//!
//! ## Drain role/primary
//! [`Changelog::drain_since`] reads bodies from the [`BodySource`] at
//! `(role, primary)`. Per ADR-0011 the body lives in *our* keyspace keyed by
//! the callRef's encoded primary: a callRef whose primary is **us** is a
//! `Pri` (reclaim) body; otherwise it is a `Bak` body we hold for that primary.
//! We resolve `(role, primary)` per callRef via [`partition_of`]. Because the
//! changelog returns one `(role, primary)` per `drain_since` call, we group the
//! due entries by their resolved `(role, primary)` and drain each group — the
//! common case (a single primary per peer-changelog) is one group.
//!
//! ## Lock discipline (ADR-0011 X8)
//! `drain_since` already reads bodies without holding the changelog or call-DB
//! lock across the body read/await (S4 guarantee). The serve-loop never holds a
//! lock across a `send`/`recv`/`notified` await.
//!
//! ## Bootstrap (S6 — the lazy-batch scan)
//! `PullMode::Bootstrap` runs [`serve_bootstrap`](ReplServer::serve_bootstrap):
//! it snapshots the `bak:{caller}` callRef KEYS under a brief lock, captures
//! `W = head` at scan start, then streams each LIVE body in `chunk` batches
//! (each body read under a SHORT lock, never held across the socket `send`) as
//! `Data{ op: Create, partition: Pri }`, terminated by `Noop{ at: W }`. The same
//! connection then loops back to recv the client's follow-up
//! `PullRequest(Replog, since=W)` and serves it via
//! [`serve_replog`](ReplServer::serve_replog) — one socket, Bootstrap → Replog.
//! Re-hydration and backup-re-subscription are the same pull stream (X4).

use std::sync::Arc;

use repl_net::frame::{Frame, Op, Partition, PullMode, Watermark};
use repl_net::transport::{ReplicationConnection, ReplicationListener};
use tokio::sync::watch;

use super::changelog::{BodySource, Changelog};
use crate::store::{partition_of, PartitionRole};

/// Default bootstrap batch size when the client's `chunk` hint is 0.
const DEFAULT_BOOTSTRAP_CHUNK: usize = 128;

/// Serves a node's changelog to pulling peers over a [`ReplicationListener`].
///
/// Cheap to clone (Arcs); `run` accepts forever and spawns a per-connection
/// task. Each task drives one caller's long-lived subscription and ends cleanly
/// when that connection is cut (`recv` → `None` / `send` → `Err`).
#[derive(Clone)]
pub struct ReplServer {
    /// This node's ordinal — used to resolve each callRef's `(role, primary)`
    /// in our keyspace ([`partition_of`]).
    self_ordinal: String,
    changelog: Changelog,
    source: Arc<dyn BodySource>,
    /// **`Deactivate` broadcast source** (ADR-0011 X11). When this node goes
    /// active after a reboot it bumps a watch with its ownership-reassertion
    /// wall-clock; every live `serve_replog` connection then pushes
    /// `Deactivate{as_of}` to its pulling backup so it hands back our takeover
    /// copies. `None` (the sim/test servers) never emits one. The value `0` means
    /// "no reclaim yet" and is never pushed.
    deactivate_rx: Option<watch::Receiver<i64>>,
}

impl ReplServer {
    /// Build a server for `self_ordinal` serving `changelog`, reading bodies
    /// from `source` (the local [`ReplicatingCallStore`]).
    ///
    /// [`ReplicatingCallStore`]: super::ReplicatingCallStore
    pub fn new(
        self_ordinal: impl Into<String>,
        changelog: Changelog,
        source: Arc<dyn BodySource>,
    ) -> Self {
        Self {
            self_ordinal: self_ordinal.into(),
            changelog,
            source,
            deactivate_rx: None,
        }
    }

    /// Attach the X11 `Deactivate` broadcast watch (ADR-0011 X11). Builder so the
    /// existing 3-arg [`new`](Self::new) test callers are unchanged; the live
    /// `B2buaCore` wires the watch its go-active task bumps.
    pub fn with_deactivate_watch(mut self, rx: watch::Receiver<i64>) -> Self {
        self.deactivate_rx = Some(rx);
        self
    }

    /// Accept connections forever, serving each on a per-connection task. Returns
    /// when the listener is closed (`accept` → `None`). Spawn this on a task.
    ///
    /// The serve tasks are owned by a [`JoinSet`](tokio::task::JoinSet), NOT
    /// detached via a bare `tokio::spawn`. This is load-bearing for crash
    /// semantics: when the worker is aborted (a simulated or real crash), this
    /// `run` future is dropped, which drops the `JoinSet` and **aborts every live
    /// serve task**, cutting their connections so each peer's `recv` returns
    /// `None` and it reconnects to the rebooted node. A detached serve task would
    /// outlive the crash, hold its connection open, and the peer would never see
    /// the cut — so it would never reconnect to the new incarnation and the X11
    /// `Deactivate` handback (and all fresh replication) would never reach it.
    pub async fn run(self, listener: Box<dyn ReplicationListener>) {
        let mut serves = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Some(conn) => {
                        let server = self.clone();
                        serves.spawn(async move {
                            server.serve_connection(conn).await;
                        });
                    }
                    None => break, // listener closed — stop accepting.
                },
                // Reap finished serve tasks so the set never grows unbounded.
                // Inert (pattern skips `None`) while the set is empty.
                Some(_) = serves.join_next() => {}
            }
        }
    }

    /// Serve one accepted connection. A connection processes a **sequence** of
    /// `PullRequest`s on the same socket:
    ///
    /// - `Bootstrap` → [`serve_bootstrap`](Self::serve_bootstrap): lazy-batch
    ///   `bak:{caller}` KEYS scan → `Data(Pri)…` → terminal `Noop(W)`, then loop
    ///   back to recv the client's NEXT request (it will be `Replog(since=W)`).
    /// - `Replog` → [`serve_replog`](Self::serve_replog): the long-lived
    ///   steady-state subscription (loops until the connection closes / errors).
    ///
    /// A bare `Replog`-first connection still works exactly as in S5 (the
    /// bootstrap branch is simply skipped). Returns when the socket closes.
    async fn serve_connection(self, conn: Box<dyn ReplicationConnection>) {
        loop {
            // Read the next opening PullRequest. Ignore (and keep reading) any
            // stray Acks; bail on close or any other frame.
            let (caller, mode, since, chunk) = loop {
                match conn.recv().await {
                    Some(Frame::PullRequest {
                        caller,
                        mode,
                        since,
                        chunk,
                        ..
                    }) => break (caller, mode, since, chunk),
                    // Optional inbound Ack handling (retention trim). No-op stub;
                    // S6+: record `up_to` to bound changelog retention.
                    Some(Frame::Ack { .. }) => continue,
                    // Any other frame before the PullRequest, or a close → done.
                    _ => return,
                }
            };

            match mode {
                PullMode::Bootstrap => {
                    // Bootstrap is one batched pass + a terminal Noop, then we
                    // loop back to recv the client's follow-up Replog request on
                    // THIS connection.
                    if self.serve_bootstrap(conn.as_ref(), &caller, chunk).await.is_err() {
                        return;
                    }
                    // → loop: recv the next (Replog) PullRequest.
                }
                PullMode::Replog => {
                    // Long-lived subscription; only returns on close/err. This is
                    // the terminal state of a connection (no further requests).
                    self.serve_replog(conn.as_ref(), &caller, since).await;
                    return;
                }
            }
        }
    }

    /// Serve a `Bootstrap` re-hydration pass for `caller` (X4 / Decision 3):
    ///
    /// 1. snapshot the `bak:{caller}` callRef KEYS under a brief lock
    ///    ([`BodySource::scan_refs`]), capture `W = changelog.head()` at scan
    ///    start;
    /// 2. for each batch of `chunk` keys, read each LIVE body under a short lock
    ///    (`read_body` + `read_meta`) and send a `Data{ op: Create, partition:
    ///    Pri, .. }` — a key whose body vanished mid-scan is sent as a `Delete`
    ///    (the client drops/ignores it; the real body, if any, re-arrives via the
    ///    tail). The store/changelog lock is NEVER held across `send().await`;
    /// 3. send the TERMINAL `Noop{ at: W }` (carries the scan-start head; the
    ///    client uses it only as the bootstrap-terminal marker — it then re-pulls
    ///    `Replog` from `(0,0)` cold for the full changelog, so this W is not the
    ///    client's resume watermark).
    ///
    /// Partition tagging: we scan our `bak:{caller}` partition (calls whose
    /// primary is `caller`, that we back up) and tag each frame `partition=Pri`
    /// so the client imports them as `pri:{caller}` — its own calls it reclaims.
    ///
    /// Returns `Err(())` if the socket dropped mid-pass (caller ends the conn).
    async fn serve_bootstrap(
        &self,
        conn: &dyn ReplicationConnection,
        caller: &str,
        chunk: u32,
    ) -> Result<(), ()> {
        // (1) snapshot keys + capture W under (separate) brief locks.
        let keys = self.source.scan_refs(PartitionRole::Backup, caller);
        let w = self.changelog.head();

        let batch = if chunk == 0 {
            DEFAULT_BOOTSTRAP_CHUNK
        } else {
            chunk as usize
        };

        // (2) stream bodies in batches; each body read is a short, lock-dropping
        // call (no lock held across the send).
        for group in keys.chunks(batch) {
            for key in group {
                let body = self
                    .source
                    .read_body(PartitionRole::Backup, caller, key)
                    .await;
                let frame = match (body, self.source.read_meta(key)) {
                    (Some(body), Some(meta)) => Frame::Data {
                        at: w,
                        op: Op::Create,
                        partition: Partition::Pri,
                        call_ref: key.clone(),
                        call_gen: meta.call_gen,
                        body_ttl_ms: meta.body_ttl_ms,
                        indexes: meta.indexes,
                        body: Some(body),
                    },
                    // Body vanished mid-scan (TTL evict / delete): tell the client
                    // to drop it rather than leak a phantom pri:{caller} entry.
                    _ => Frame::Data {
                        at: w,
                        op: Op::Delete,
                        partition: Partition::Pri,
                        call_ref: key.clone(),
                        call_gen: 0,
                        body_ttl_ms: 0,
                        indexes: Vec::new(),
                        body: None,
                    },
                };
                conn.send(frame).await.map_err(|_| ())?;
            }
        }

        // (3) TERMINAL marker — end of bootstrap; carries W (scan-start head) so
        // the client seeds its Replog watermark to it.
        conn.send(Frame::Noop { at: w }).await.map_err(|_| ())?;
        Ok(())
    }

    /// Serve the steady-state `Replog` subscription for `caller` from `since`:
    /// push drained `Data` + `Noop(head)`, then park on the changelog subscriber
    /// until a new bump or the connection closes. Loops until the socket ends.
    async fn serve_replog(
        &self,
        conn: &dyn ReplicationConnection,
        caller: &str,
        mut w: Watermark,
    ) {
        // If the caller's `since` fell below the compacted/reaped tail it has
        // missed a now-vanished mutation (e.g. a tombstone reaped during a long
        // disconnect). Tell it to re-bootstrap rather than silently diverge.
        if self.changelog.needs_reset(caller, w) {
            let _ = conn
                .send(Frame::ResetToBootstrap {
                    reason: "since fell below the compacted tail".into(),
                })
                .await;
            return;
        }

        // Subscribe BEFORE the first drain so a bump racing the drain is not
        // missed (edge-triggered Notify; subscribing first arms the permit). The
        // returned guard keeps the peer log reap-immune for this loop's lifetime.
        let sub = self.changelog.subscribe(caller);

        // X11 `Deactivate` broadcast: a clone of the go-active watch (if wired).
        let mut deact = self.deactivate_rx.clone();
        // Reconnect backstop — if we already reclaimed (watch > 0), tell this
        // (re)subscribing backup to hand back our stale takeover copies at once.
        if let Some(rx) = &deact {
            let as_of = *rx.borrow();
            if as_of > 0 && conn.send(Frame::Deactivate { as_of_ms: as_of }).await.is_err() {
                return;
            }
        }

        loop {
            // Drain everything strictly above `w`, send ascending.
            let frames = self.drain(caller, w).await;
            for frame in frames {
                if let Frame::Data { at, .. } = &frame {
                    let at = *at;
                    if conn.send(frame).await.is_err() {
                        return; // connection cut — end the task cleanly.
                    }
                    w = at; // advance past what we just sent.
                } else if conn.send(frame).await.is_err() {
                    return;
                }
            }

            // Caught up: advance to head and send the Noop marker.
            let head = self.changelog.head();
            if head > w {
                w = head;
            }
            if conn.send(Frame::Noop { at: head }).await.is_err() {
                return;
            }

            // Park until a new changelog bump, a `Deactivate` broadcast, or the
            // connection closes, whichever first.
            tokio::select! {
                _ = sub.notified() => {}
                // X11: the node went active / re-sent its handback — push the
                // current ownership-reassertion instant to this backup. The arm
                // is INERT (parks forever) in two cases: `deact` is `None` (no
                // watch wired — the sim/test path), or the watch has CLOSED. The
                // latter is the trap: the sole `deactivate_tx` (b2bua_core.rs)
                // is dropped when the go-active task's ~5 s reclaim burst ends,
                // after which `changed()` resolves `Err` IMMEDIATELY and FOREVER.
                // Keep polling it and the `select!` never parks — a permanent
                // ~100 % CPU spin per backup connection (the very regression this
                // comment guards). So on close we drop `deact` to `None` and fall
                // through to the `pending` arm next iteration. The last as_of
                // still persists for the reconnect backstop above (a closed
                // `watch::Receiver` can still `borrow()`), so no handback is lost.
                res = async {
                    match deact.as_mut() {
                        Some(rx) => rx.changed().await.map(|_| *rx.borrow()).ok(),
                        None => std::future::pending::<Option<i64>>().await,
                    }
                } => {
                    match res {
                        // A real ownership-reassertion instant — relay it.
                        Some(as_of) => {
                            if as_of > 0 && conn.send(Frame::Deactivate { as_of_ms: as_of }).await.is_err() {
                                return;
                            }
                        }
                        // `res` is `None` ONLY via the `Some(rx) => Err` (closed)
                        // path — the `None`/`pending` arm never resolves. Disable
                        // the arm so the next loop parks instead of busy-looping.
                        None => deact = None,
                    }
                }
                // A peer that cuts the connection wakes recv with None; end.
                msg = conn.recv() => match msg {
                    // Inbound Ack mid-stream: retention-trim hint. No-op stub.
                    Some(Frame::Ack { .. }) => {}
                    // Any other inbound frame is unexpected on a live sub; ignore.
                    Some(_) => {}
                    None => return,
                },
            }
        }
    }

    /// Drain due entries for `caller` above `since`, resolving each callRef's
    /// `(role, primary)` in our keyspace. Returns frames in ascending `at`.
    async fn drain(&self, caller: &str, since: Watermark) -> Vec<Frame> {
        // Group the due callRefs by their resolved (role, primary) so each
        // `drain_since` call reads bodies from the right keyspace. In the common
        // case every ref shares one primary → a single group.
        let mut groups: Vec<Group> = Vec::new();
        for call_ref in self.changelog.due_refs(caller, since) {
            let g = partition_of(&self.self_ordinal, &call_ref);
            if !groups.contains(&g) {
                groups.push(g);
            }
        }
        if groups.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (role, primary) in groups {
            let mut frames = self
                .changelog
                .drain_since(caller, since, self.source.as_ref(), role, &primary)
                .await;
            // `drain_since` returns ALL due refs for this since; keep only those
            // whose body lives in this (role, primary) group.
            frames.retain(|f| matches!(f, Frame::Data { call_ref, .. }
                if partition_of(&self.self_ordinal, call_ref) == (role, primary.clone())));
            out.append(&mut frames);
        }
        // Sort ascending by watermark so the serve-loop advances W monotonically.
        out.sort_by_key(|f| match f {
            Frame::Data { at, .. } => *at,
            _ => Watermark::new(0, 0),
        });
        out
    }

    /// Test accessor: this node's ordinal.
    #[cfg(test)]
    pub fn self_ordinal(&self) -> &str {
        &self.self_ordinal
    }
}

/// `(role, primary)` keyspace key the server groups due callRefs by.
type Group = (crate::store::PartitionRole, String);

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use repl_net::transport::SendError;
    use sip_clock::Clock;
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// An idle, *live* backup connection: `send` records each frame (and yields,
    /// faithful to a real socket write, so a spinning loop still interleaves with
    /// the test driver); `recv` parks forever (a quiet, connected backup). With
    /// this connection the ONLY thing that can drive `serve_replog`'s loop is the
    /// `Deactivate` watch arm — so the `send` count is a direct spin detector.
    struct IdleConn {
        sends: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl ReplicationConnection for IdleConn {
        async fn send(&self, _frame: Frame) -> Result<(), SendError> {
            self.sends.fetch_add(1, Ordering::Relaxed);
            tokio::task::yield_now().await;
            Ok(())
        }
        async fn recv(&self) -> Option<Frame> {
            std::future::pending().await
        }
        fn peer_addr(&self) -> SocketAddr {
            "127.0.0.1:9".parse().unwrap()
        }
        fn local_addr(&self) -> SocketAddr {
            "127.0.0.1:8".parse().unwrap()
        }
    }

    /// Regression (CPU spin): a CLOSED `Deactivate` watch must not busy-loop
    /// `serve_replog`. The sole `deactivate_tx` (b2bua_core.rs go-active task) is
    /// dropped when its ~5 s reclaim burst ends; thereafter `changed()` resolves
    /// `Err` immediately and forever. Before the fix the `select!` re-polled that
    /// arm every turn and never parked — a permanent ~100 % CPU spin per backup
    /// connection (2.5–4.7 cores observed in-cluster at 50 cps with *zero* repl
    /// traffic). The fix disables the arm on close. We detect a spin by counting
    /// `Noop` sends: a caught-up `serve_replog` emits one per loop iteration, so a
    /// parked loop sends ~1–2 then goes quiet while a spin sends one per turn,
    /// unboundedly.
    #[tokio::test]
    async fn closed_deactivate_watch_does_not_spin_serve_replog() {
        let clock = Clock::test_at(0);
        let store = Arc::new(crate::repl::ReplicatingCallStore::new(0, clock.clone()));
        let changelog = Changelog::new(0, clock);
        // Close the watch up front — the steady state once the go-active task has
        // finished its burst and dropped the only sender.
        let (tx, rx) = watch::channel(0i64);
        drop(tx);
        let server = ReplServer::new("self", changelog, store).with_deactivate_watch(rx);

        let sends = Arc::new(AtomicUsize::new(0));
        let conn = IdleConn { sends: sends.clone() };
        let task = tokio::spawn(async move {
            server
                .serve_replog(&conn, "peer", Watermark { gen: 0, counter: 0 })
                .await;
        });

        // Yield generously so a spinning loop has ample turns to reveal itself.
        for _ in 0..1000 {
            tokio::task::yield_now().await;
        }
        let n = sends.load(Ordering::Relaxed);
        task.abort();

        assert!(
            n <= 8,
            "serve_replog spun on a closed Deactivate watch: {n} Noop sends over \
             1000 scheduler turns (a parked loop sends ~1–2; a spin sends hundreds)"
        );
    }
}
