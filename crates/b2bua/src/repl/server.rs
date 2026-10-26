//! [`ReplServer`] — the replication **serve-loop** (ADR-0014 §Stream topology).
//!
//! A node `listen`s on its replication address and serves each inbound
//! connection **one flow** (the `partition` on the opening `PullRequest` selects
//! Reclaim vs Backup). There is no multiplexing and no second request: one
//! socket = one flow.
//!
//! ```text
//! peer → us:  PullRequest{ proto_ver, partition, since } # ONCE, opens one flow
//! us → peer:  StreamRefused{ proto_ver }                 # another version: closed
//! us → peer:  [bootstrap store-scan if since==(0,0)]     # snapshot, stamped at W=head
//!             Data…(entries > W, batched) ; Noop(head)   # drain, catch-up Noop
//!             …wait for a write; drain again; Noop only on the catch-up edge + idle interval…
//! peer → us:  Position{ at }                             # what the peer has applied
//! peer → us:  ReplicaRefused{ at, call_ref }             # a replica the peer will not hold
//! ```
//!
//! ## One protocol version
//! A `PullRequest` announcing another [`PROTO_VER`] is answered with one
//! [`Frame::StreamRefused`] and nothing else: no frame of either version could
//! be read by the other.
//!
//! ## The peer's back-channel
//! A separate per-connection task is the only reader past the opening request: it
//! records each [`Frame::Position`] on the flow's sub-log (ADR-0031 D2), hands
//! each [`Frame::ReplicaRefused`] to the store (the call has no backup from
//! then on, ADR-0055 §3) and, on a close, flags the serve loop to end. It exists
//! because a real socket's `recv` is not cancel-safe mid-read, so it can never
//! be an arm of the serve loop's `select!`. Any other frame from the client is
//! ignored.
//!
//! ## Sent on write (ADR-0055 §3)
//! The server drains up to [`BATCH`] entries above its cursor each time a write
//! lands in the flow's sub-log. While a full batch comes back it loops
//! immediately (fill the TCP buffer); below a batch it waits for the next write.
//! A **`Noop`** is sent only on the **catch-up edge** (just finished draining a
//! backlog) and every [`ServeConfig::noop_interval`] after the last one — the
//! *first* one tells the puller it is current, the later ones tell it the stream
//! is alive.
//!
//! ## A peer that stops reading (ADR-0045)
//! Every send is bounded by [`ServeConfig::send_stall`]: a send still blocked
//! then (the peer's window full, nothing read) drops the stream, which ends the
//! serve task and releases the changelog its guard pinned.
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

use repl_net::frame::{Frame, Op, Partition, Watermark, PROTO_VER};
use repl_net::transport::{AcceptError, ReplicationConnection, ReplicationListener, SendError};

use super::changelog::{BodySource, Changelog};
use crate::abort_on_drop::AbortOnDrop;
use crate::store::PartitionRole;

/// Max entries drained + sent per drain. A full batch means "more pending" →
/// the loop streams the next batch immediately (filling the TCP buffer); a short
/// batch means caught-up → emit the catch-up Noop and wait for a write.
const BATCH: usize = 500;

/// The pause after a failed accept before the next one.
const ACCEPT_PAUSE: Duration = Duration::from_millis(100);

/// The link bounds of the serve loop (ADR-0045).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServeConfig {
    /// The longest a caught-up stream goes without a `Noop`.
    pub noop_interval: Duration,
    /// A send blocked this long drops the stream.
    pub send_stall: Duration,
}

impl Default for ServeConfig {
    fn default() -> Self {
        Self { noop_interval: Duration::from_secs(5), send_stall: Duration::from_secs(15) }
    }
}

impl ServeConfig {
    /// How long a puller's position claim vouches for its flow: two `Noop`
    /// intervals, each `Noop` drawing a fresh claim (ADR-0031 D2).
    pub fn claim_ttl(&self) -> Duration {
        self.noop_interval * 2
    }
}

/// Why a send did not complete.
#[derive(Debug)]
pub(super) enum Unsent {
    /// The stream closed.
    Closed,
    /// The send stayed blocked past [`ServeConfig::send_stall`].
    Stalled,
    /// The transport refused the frame, or a frame of the batch, unsent: it
    /// does not encode or passes the frame cap. The stream stays usable.
    Refused(String),
}

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
    config: ServeConfig,
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
            config: ServeConfig::default(),
        }
        .with_config(ServeConfig::default())
    }

    /// Serve under `config`'s link bounds; the changelog's claims last
    /// [`ServeConfig::claim_ttl`].
    pub fn with_config(mut self, config: ServeConfig) -> Self {
        self.changelog.set_claim_ttl(config.claim_ttl());
        self.config = config;
        self
    }

    /// Attach the metrics handle so the serve loop records `repl_noops_sent`
    /// (builder, so the existing [`new`](Self::new) test callers are unchanged).
    pub fn with_metrics(mut self, metrics: crate::metrics::B2buaMetrics) -> Self {
        self.metrics = Some(metrics);
        self
    }

    /// Accept connections forever, serving each on a per-connection task. Returns
    /// only when the listener is closed ([`AcceptError::Closed`]). A failed
    /// accept (descriptors exhausted, a handshake aborted) never stops it: a
    /// warning once per run of failures, a pause of 100 ms, and
    /// accepting goes on. Spawn this on a task.
    ///
    /// The serve tasks are owned by a [`JoinSet`](tokio::task::JoinSet), NOT
    /// detached. This is load-bearing for crash semantics: when the worker is
    /// aborted, this `run` future is dropped, dropping the `JoinSet` and aborting
    /// every live serve task, cutting their connections so each peer reconnects
    /// to the rebooted node.
    pub async fn run(self, listener: Box<dyn ReplicationListener>) {
        let mut serves = tokio::task::JoinSet::new();
        let mut failing = false;
        loop {
            tokio::select! {
                accepted = listener.accept() => match accepted {
                    Ok(conn) => {
                        if std::mem::take(&mut failing) {
                            tracing::info!(
                                node = observe::node(),
                                local = %listener.local_addr(),
                                "replication listener accepting again"
                            );
                        }
                        let server = self.clone();
                        serves.spawn(observe::keep_node(async move {
                            server.serve_connection(conn).await;
                        }));
                    }
                    Err(AcceptError::Closed) => break,
                    Err(AcceptError::Io(error)) => {
                        if !std::mem::replace(&mut failing, true) {
                            tracing::warn!(
                                node = observe::node(),
                                local = %listener.local_addr(),
                                %error,
                                "replication accept failed; accepting again after a pause"
                            );
                        }
                        tokio::time::sleep(ACCEPT_PAUSE).await;
                    }
                },
                Some(_) = serves.join_next() => {}
            }
        }
    }

    /// Serve one accepted connection: recv the opening `PullRequest`, then drive
    /// its single flow to completion (until the socket closes).
    async fn serve_connection(self, conn: Box<dyn ReplicationConnection>) {
        let conn: Arc<dyn ReplicationConnection> = Arc::from(conn);
        let (caller, partition, since) = match conn.recv().await {
            Some(Frame::PullRequest { proto_ver, caller, partition, since })
                if proto_ver == PROTO_VER =>
            {
                (caller, partition, since)
            }
            Some(Frame::PullRequest { proto_ver, caller, .. }) => {
                tracing::error!(
                    node = observe::node(),
                    peer = %caller,
                    ours = PROTO_VER,
                    theirs = proto_ver,
                    "replication stream refused: the peer speaks another protocol"
                );
                let reason = format!("this node speaks replication protocol {PROTO_VER}");
                let refused = Frame::StreamRefused { proto_ver: PROTO_VER, reason };
                let _ = self.bounded(conn.send(refused), &caller, "refused").await;
                return;
            }
            // Anything other than an opening PullRequest, or a close → done.
            _ => return,
        };
        self.serve_flow(conn, &caller, partition, since).await;
    }

    /// `(role, primary)` body keyspace for a flow (fixed by the partition):
    /// Reclaim ⇒ `bak:{caller}`, Backup ⇒ `pri:{self}`.
    fn keyspace(&self, partition: Partition, caller: &str) -> (PartitionRole, String) {
        match partition {
            Partition::Pri => (PartitionRole::Backup, caller.to_string()),
            Partition::Bak => (PartitionRole::Primary, self.self_ordinal.clone()),
        }
    }

    /// Drive one flow: optional bootstrap store-scan (cold), then the tail sent
    /// on write.
    async fn serve_flow(
        &self,
        conn: Arc<dyn ReplicationConnection>,
        caller: &str,
        partition: Partition,
        since: Watermark,
    ) {
        // Keep this peer's changelog reap-immune, and this partition's flow
        // marked connected, while we serve it.
        let mut guard = self.changelog.serving(caller, partition);
        // The peer's back-channel: positions in, and a close that ends us.
        let (closed_tx, mut closed) = tokio::sync::watch::channel(false);
        let _recv = AbortOnDrop(observe::spawn_keeping_node({
            let conn = conn.clone();
            let changelog = self.changelog.clone();
            let source = self.source.clone();
            let peer = caller.to_string();
            async move {
                while let Some(frame) = conn.recv().await {
                    match frame {
                        Frame::Position { at } => changelog.note_applied(&peer, partition, at),
                        Frame::ReplicaRefused { call_ref, .. } if partition == Partition::Bak => {
                            source.backup_refused(&peer, &call_ref);
                        }
                        _ => {}
                    }
                }
                let _ = closed_tx.send(true);
            }
        }));
        let conn = conn.as_ref();
        let (role, primary) = self.keyspace(partition, caller);
        // Stream-kind label for the per-(flow, peer) Noop-sent liveness counter.
        let flow = match partition {
            Partition::Pri => "reclaim",
            Partition::Bak => "backup",
        };

        // A warm puller that fell below the compacted tail must re-bootstrap.
        if self.changelog.needs_reset(caller, partition, since) {
            let reason = "since fell below the compacted tail".into();
            let _ = self.send(conn, Frame::ResetToBootstrap { reason }, caller, flow).await;
            return;
        }

        // Cold (since==(0,0)) ⇒ bulk store-scan; W = head at scan start. The tail
        // then resumes from W (warm), so the scan and the tail never double-deliver.
        let mut w = since;
        if since == Watermark::new(0, 0) {
            match self.serve_bootstrap(conn, caller, partition, role, &primary, flow).await {
                Ok(scan_head) => w = scan_head,
                Err(_) => return, // socket dropped or stalled mid-scan.
            }
        }

        // ---- tail, drained on write ----
        let mut ever_caught_up = false;
        let mut streamed_full_batch = false;
        let mut next_noop = sip_clock::deadline::from_now(self.config.noop_interval);
        loop {
            let frames = self
                .changelog
                .drain_since(caller, partition, w, BATCH, self.source.as_ref(), role, &primary)
                .await;
            let n = frames.len();
            for frame in frames {
                let at = match &frame {
                    Frame::Data { at, .. } => Some(*at),
                    _ => None,
                };
                if self.send_or_skip(conn, frame, caller, flow).await.is_err() {
                    return;
                }
                if let Some(at) = at {
                    w = at; // advance past what we just sent.
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
            // backlog) and every `noop_interval` after the last one, so the
            // puller's idle bound never fires on a live stream.
            let edge = !ever_caught_up || streamed_full_batch;
            if edge || tokio::time::Instant::now() >= next_noop {
                if self.send(conn, Frame::Noop { at: w }, caller, flow).await.is_err() {
                    return;
                }
                if let Some(m) = &self.metrics {
                    m.record_repl_noop_sent(flow, caller);
                }
                ever_caught_up = true;
                streamed_full_batch = false;
                next_noop = sip_clock::deadline::from_now(self.config.noop_interval);
            }
            // Each arm is cancel-safe, so the peer's close can race them here;
            // polled in order, so a run is deterministic.
            tokio::select! {
                biased;
                _ = closed.changed() => return,
                () = guard.written() => {}
                () = tokio::time::sleep_until(next_noop) => {}
            }
        }
    }

    /// Send `frame`; one the transport refuses is logged, counted and skipped,
    /// so the stream goes on past it, and the call of a refused Backup `Put`
    /// has no backup from then on.
    pub(super) async fn send_or_skip(
        &self,
        conn: &dyn ReplicationConnection,
        frame: Frame,
        caller: &str,
        flow: &'static str,
    ) -> Result<(), Unsent> {
        let (call_ref, backed_up) = match &frame {
            Frame::Data { call_ref, partition, op, .. } => {
                (call_ref.clone(), *partition == Partition::Bak && *op == Op::Put)
            }
            _ => (String::new(), false),
        };
        match self.send(conn, frame, caller, flow).await {
            Err(Unsent::Refused(why)) => {
                if let Some(m) = &self.metrics {
                    m.bump_repl_frame_refused();
                }
                tracing::error!(
                    node = observe::node(),
                    peer = %caller,
                    flow,
                    call_ref,
                    why,
                    "replication frame refused unsent; the stream goes on past it"
                );
                // A call whose write cannot reach its backup has none from
                // here on, never a backup that silently lacks it.
                if backed_up {
                    self.source.abandon_backup(caller, &call_ref);
                }
                Ok(())
            }
            sent => sent,
        }
    }

    /// Send `frame`, bounded by [`ServeConfig::send_stall`].
    async fn send(
        &self,
        conn: &dyn ReplicationConnection,
        frame: Frame,
        caller: &str,
        flow: &'static str,
    ) -> Result<(), Unsent> {
        self.bounded(conn.send(frame), caller, flow).await
    }

    /// Await a send for at most [`ServeConfig::send_stall`]. A send still
    /// blocked then is a peer that stopped reading: the caller drops the stream.
    pub(super) async fn bounded(
        &self,
        send: impl std::future::Future<Output = Result<(), repl_net::transport::SendError>>,
        caller: &str,
        flow: &'static str,
    ) -> Result<(), Unsent> {
        match tokio::time::timeout(self.config.send_stall, send).await {
            Ok(Ok(())) => Ok(()),
            Ok(Err(SendError::Unencodable(why))) => Err(Unsent::Refused(why)),
            Ok(Err(_)) => Err(Unsent::Closed),
            Err(_) => {
                tracing::info!(
                    node = observe::node(),
                    peer = %caller,
                    flow,
                    stall_ms = sip_clock::wall::millis_u64(self.config.send_stall),
                    "replication stream dropped: the peer stopped reading"
                );
                Err(Unsent::Stalled)
            }
        }
    }

    /// Test accessor: this node's ordinal.
    #[cfg(test)]
    pub fn self_ordinal(&self) -> &str {
        &self.self_ordinal
    }
}

mod bootstrap;
