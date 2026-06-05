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
        }
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
    /// the cut — so it would never reconnect to the new incarnation and fresh
    /// replication (the rebooted primary's forward refreshes) would never reach it.
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
    ///    Pri, .. }` — a key whose body vanished between the snapshot and the read
    ///    (TTL evict / concurrent delete) is **skipped** (the call ended; a
    ///    synthesised `Delete` could only tear down a live copy on an overlapping
    ///    re-bootstrap, never resurrect a dead one). The store/changelog lock is
    ///    NEVER held across `send().await`;
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
        // call (no lock held across the send). We read a whole `chunk` of bodies
        // then push them in ONE `send_batch` — a single write + flush per chunk
        // rather than per body. The per-frame flush was the bootstrap throughput
        // wall on any non-loopback link (each tiny body became its own
        // write+flush syscall / TCP segment, fully serialised on the write lock);
        // batching amortises it ~`chunk`-fold. The wire is unchanged.
        for group in keys.chunks(batch) {
            let mut frames = Vec::with_capacity(group.len());
            for key in group {
                let body = self
                    .source
                    .read_body(PartitionRole::Backup, caller, key)
                    .await;
                match (body, self.source.read_meta(key)) {
                    (Some(body), Some(meta)) => frames.push(Frame::Data {
                        at: w,
                        op: Op::Create,
                        partition: Partition::Pri,
                        call_ref: key.clone(),
                        call_gen: meta.call_gen,
                        call_bgen: meta.call_bgen,
                        body_ttl_ms: meta.body_ttl_ms,
                        indexes: meta.indexes,
                        body: Some(body),
                    }),
                    // Body vanished between the keyset snapshot and this read (TTL
                    // evict / concurrent delete) — the call genuinely ended. SKIP
                    // it; do NOT synthesise a `Pri` Delete. On the reclaiming node
                    // a Delete is a no-op when cold (nothing to remove) but an
                    // active teardown if a re-bootstrap overlaps a copy another
                    // path already reclaimed — i.e. it can only delete a live call,
                    // never resurrect a dead one. A genuinely-live call is never
                    // unreadable (a non-expired body always reads), so skipping
                    // loses nothing real.
                    _ => continue,
                }
            }
            conn.send_batch(frames).await.map_err(|_| ())?;
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

            // Park until a new changelog bump or the connection closes.
            tokio::select! {
                _ = sub.notified() => {}
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

