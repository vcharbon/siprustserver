//! [`ReplServer`] — the replication **serve-loop** (ADR-0014 §Stream topology).
//!
//! A node `listen`s on its replication address and serves each inbound
//! connection **one flow** (the `partition` on the opening `PullRequest` selects
//! Reclaim vs Backup). There is no multiplexing and no second request: one
//! socket = one flow.
//!
//! ```text
//! peer → us:  PullRequest{ partition, since }          # ONCE, opens one flow
//! us → peer:  [bootstrap store-scan if since==(0,0)]    # snapshot, stamped at W=head
//!             Data…(entries > W, batched) ; Noop(head)  # poll-drain, catch-up Noop
//!             …sleep ~100ms; drain again; Noop only on the catch-up edge + 20s idle…
//! ```
//!
//! ## Pure poll, no `Notify`
//! The server is a `sleep`-poll loop (ADR-0014): every ~100ms it drains up to
//! [`BATCH`] entries above its cursor. While a full batch comes back it loops
//! immediately (fill the TCP buffer); below a batch it sleeps. A **`Noop`** is
//! sent only on the **catch-up edge** (just finished draining a backlog) and on
//! a ~20s **idle floor** — the *first* one tells the puller it is current.
//!
//! ## Flow → keyspace
//! `partition` fixes the body keyspace (no per-callRef `partition_of`):
//! - **Reclaim** (`Pri`): caller reclaims its own calls we hold → `bak:{caller}`.
//! - **Backup** (`Bak`): caller backs up our own calls → `pri:{self}` (filtered
//!   to the calls `caller` backs up — ADR-0014 Option B).
//!
//! ## Lock discipline (ADR-0011 X8)
//! `drain_since` reads bodies without holding the changelog or call-DB lock
//! across the read/await; the serve-loop never holds a lock across a `send`.

use std::sync::Arc;
use std::time::Duration;

use repl_net::frame::{Frame, Partition, Watermark};
use repl_net::transport::{ReplicationConnection, ReplicationListener};

use super::changelog::{BodySource, Changelog};
use crate::store::PartitionRole;

/// Bootstrap store-scan batch size (bodies per `send_batch`).
const BOOTSTRAP_CHUNK: usize = 128;

/// Max entries drained + sent per poll tick. A full batch means "more pending" →
/// the loop streams the next batch immediately (filling the TCP buffer); a short
/// batch means caught-up → emit the catch-up Noop and sleep.
const BATCH: usize = 500;

/// Poll cadence between drains when not back-to-back streaming (tunable lower).
const POLL: Duration = Duration::from_millis(100);

/// Idle poll cycles between keepalive `Noop`s once fully caught up (≈20s @ 100ms).
const IDLE_NOOP_CYCLES: u32 = 200;

/// Serves a node's changelog to pulling peers over a [`ReplicationListener`].
///
/// Cheap to clone (Arcs); `run` accepts forever and spawns a per-connection
/// task. Each task drives one caller's single-flow stream and ends cleanly when
/// that connection is cut (`recv` → `None` / `send` → `Err`).
#[derive(Clone)]
pub struct ReplServer {
    /// This node's ordinal — the `pri:{self}` primary for the Backup flow.
    self_ordinal: String,
    changelog: Changelog,
    source: Arc<dyn BodySource>,
    /// Serve-side observability (the per-stream `Noop`-sent liveness counter).
    /// `None` for the sim/unit-test constructors (which assert on the store
    /// directly); the live `B2buaCore` wires it via [`with_metrics`](Self::with_metrics).
    metrics: Option<crate::metrics::B2buaMetrics>,
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
            metrics: None,
        }
    }

    /// Attach the metrics handle so the serve loop records `repl_noops_sent`
    /// (builder, so the existing [`new`](Self::new) test callers are unchanged).
    pub fn with_metrics(mut self, metrics: crate::metrics::B2buaMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Accept connections forever, serving each on a per-connection task. Returns
    /// when the listener is closed (`accept` → `None`). Spawn this on a task.
    ///
    /// The serve tasks are owned by a [`JoinSet`](tokio::task::JoinSet), NOT
    /// detached. This is load-bearing for crash semantics: when the worker is
    /// aborted, this `run` future is dropped, dropping the `JoinSet` and aborting
    /// every live serve task, cutting their connections so each peer reconnects
    /// to the rebooted node.
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
                Some(_) = serves.join_next() => {}
            }
        }
    }

    /// Serve one accepted connection: recv the opening `PullRequest`, then drive
    /// its single flow to completion (until the socket closes).
    async fn serve_connection(self, conn: Box<dyn ReplicationConnection>) {
        let (caller, partition, since) = match conn.recv().await {
            Some(Frame::PullRequest {
                caller,
                partition,
                since,
                ..
            }) => (caller, partition, since),
            // Anything other than an opening PullRequest, or a close → done.
            _ => return,
        };
        self.serve_flow(conn.as_ref(), &caller, partition, since).await;
    }

    /// `(role, primary)` body keyspace for a flow (fixed by the partition):
    /// Reclaim ⇒ `bak:{caller}`, Backup ⇒ `pri:{self}`.
    fn keyspace(&self, partition: Partition, caller: &str) -> (PartitionRole, String) {
        match partition {
            Partition::Pri => (PartitionRole::Backup, caller.to_string()),
            Partition::Bak => (PartitionRole::Primary, self.self_ordinal.clone()),
        }
    }

    /// Drive one flow: optional bootstrap store-scan (cold), then the poll-tail.
    async fn serve_flow(
        &self,
        conn: &dyn ReplicationConnection,
        caller: &str,
        partition: Partition,
        since: Watermark,
    ) {
        // Keep this peer's changelog reap-immune while we serve it.
        let _guard = self.changelog.serving(caller);
        let (role, primary) = self.keyspace(partition, caller);
        // Stream-kind label for the per-(flow, peer) Noop-sent liveness counter.
        let flow = match partition {
            Partition::Pri => "reclaim",
            Partition::Bak => "backup",
        };

        // A warm puller that fell below the compacted tail must re-bootstrap.
        if self.changelog.needs_reset(caller, partition, since) {
            let _ = conn
                .send(Frame::ResetToBootstrap {
                    reason: "since fell below the compacted tail".into(),
                })
                .await;
            return;
        }

        // Cold (since==(0,0)) ⇒ bulk store-scan; W = head at scan start. The tail
        // then resumes from W (warm), so the scan and the tail never double-deliver.
        let mut w = since;
        if since == Watermark::new(0, 0) {
            match self
                .serve_bootstrap(conn, caller, partition, role, &primary)
                .await
            {
                Ok(scan_head) => w = scan_head,
                Err(()) => return, // socket dropped mid-scan.
            }
        }

        // ---- poll-tail ----
        let mut ever_caught_up = false;
        let mut streamed_full_batch = false;
        let mut idle_cycles = 0u32;
        loop {
            let frames = self
                .changelog
                .drain_since(caller, partition, w, BATCH, self.source.as_ref(), role, &primary)
                .await;
            let n = frames.len();
            for frame in frames {
                if let Frame::Data { at, .. } = &frame {
                    let at = *at;
                    if conn.send(frame).await.is_err() {
                        return;
                    }
                    w = at; // advance past what we just sent.
                } else if conn.send(frame).await.is_err() {
                    return;
                }
            }
            if n == BATCH {
                // Full batch → more likely pending: stream the next immediately.
                streamed_full_batch = true;
                continue;
            }

            // Caught up. The cursor (and the Noop it rides on) advances ONLY past
            // entries actually sent — NEVER to a separately-read `head()`.
            // `drain_since` snapshots under the changelog lock and reads bodies
            // with it dropped, so a concurrent bump for this (peer, partition)
            // could land between the snapshot and a later head read; jumping `w`
            // over it excluded the entry from every future drain
            // (`Bound::Excluded(w)`) and the Noop persisted the too-high
            // watermark across reconnects. A skipped Put self-healed on the
            // call's next flush; a skipped DELETE never re-bumps — the backup
            // held the dead call until its body-TTL backstop.
            //
            // Noop on the catch-up edge (first-ever, or after draining a real
            // backlog) and on the idle floor; a trickle needs none (its Data
            // frames already carried `at`).
            if !ever_caught_up || streamed_full_batch {
                if conn.send(Frame::Noop { at: w }).await.is_err() {
                    return;
                }
                if let Some(m) = &self.metrics {
                    m.record_repl_noop_sent(flow, caller);
                }
                ever_caught_up = true;
                streamed_full_batch = false;
                idle_cycles = 0;
            } else {
                idle_cycles += 1;
                if idle_cycles >= IDLE_NOOP_CYCLES {
                    if conn.send(Frame::Noop { at: w }).await.is_err() {
                        return;
                    }
                    if let Some(m) = &self.metrics {
                        m.record_repl_noop_sent(flow, caller);
                    }
                    idle_cycles = 0;
                }
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Bulk store-scan bootstrap for one flow. Snapshots the flow's keyset under
    /// brief locks, captures `W = head` at scan start, streams each live body as
    /// `Data{ op: Put, partition, at: W }` in [`BOOTSTRAP_CHUNK`] batches (one
    /// `send_batch` per chunk — never holding a lock across the socket), and
    /// returns `W` so the tail resumes warm. A key whose body vanished between
    /// the snapshot and the read is skipped (the call ended). `Err(())` if the
    /// socket dropped mid-pass.
    async fn serve_bootstrap(
        &self,
        conn: &dyn ReplicationConnection,
        caller: &str,
        partition: Partition,
        role: PartitionRole,
        primary: &str,
    ) -> Result<Watermark, ()> {
        // W = head at scan-START (ADR-0014 §8) — read BEFORE the key snapshot.
        // The writer order is body/meta-insert → changelog bump, so a call that
        // lands after this read necessarily bumps a counter > W and the tail
        // drains it; at worst the scan ALSO carries it (harmless double-deliver,
        // the apply side is idempotent). Reading head AFTER the scan opened the
        // reverse window — a call inserted between the scan snapshot and the
        // head read was in neither the scan nor the tail (`Bound::Excluded(W)`),
        // and if it then quiesced the backup never held it.
        let w = self.changelog.head();
        let keys = match partition {
            // Reclaim: the caller's own calls we hold as backup.
            Partition::Pri => self.source.scan_refs(PartitionRole::Backup, caller),
            // Backup: our own calls the caller backs up (Option B).
            Partition::Bak => self.source.scan_refs_backed_by(&self.self_ordinal, caller),
        };

        for group in keys.chunks(BOOTSTRAP_CHUNK) {
            let mut frames = Vec::with_capacity(group.len());
            for key in group {
                // Send-time origin stamp for skew re-anchoring on the receiver
                // (mirrors `drain_since`); one clock reading per bootstrap frame.
                let origin_now_ms = self.changelog.now_ms();
                let body = self.source.read_body(role, primary, key).await;
                match (body, self.source.read_meta(key)) {
                    (Some(body), Some(meta)) => frames.push(Frame::Data {
                        at: w,
                        op: repl_net::frame::Op::Put,
                        partition,
                        call_ref: key.clone(),
                        call_gen: meta.call_gen,
                        call_bgen: meta.call_bgen,
                        body_ttl_ms: meta.body_ttl_ms,
                        origin_now_ms,
                        indexes: meta.indexes,
                        body: Some(body),
                    }),
                    _ => continue,
                }
            }
            conn.send_batch(frames).await.map_err(|_| ())?;
        }
        Ok(w)
    }

    /// Test accessor: this node's ordinal.
    #[cfg(test)]
    pub fn self_ordinal(&self) -> &str {
        &self.self_ordinal
    }
}
