//! `RecordingReplicationNetwork` — a decorator that tees every decoded frame
//! into a shared capture sink for the S9 report.
//!
//! Wraps any [`ReplicationNetwork`]; every connection it yields — client (via
//! `connect`) or server (via the wrapped listener's `accept`) — is itself
//! wrapped so each `send`/`recv` records a decoded [`CapturedFrame`] into a
//! shared sink, stamped with the injected [`Clock`]'s timestamp. This is the
//! raw feed the report will project onto the scenario-harness renderers; the
//! renderer itself is **not** built here (later slice).
//!
//! Mirrors `sip-net`'s recording decorator pattern (decorator struct
//! implementing the same trait), but minimal: just capture + decode, no audit
//! rules / severity ledger.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use sip_clock::Clock;

use crate::Frame;

use super::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError,
};

/// Whether a captured frame was sent or received from the wrapped connection's
/// point of view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// The frame was handed to `send`.
    Sent,
    /// The frame came out of `recv`.
    Received,
}

/// One captured replication frame, decoded, with endpoints + timestamp. The raw
/// feed the report projects onto the wire renderers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapturedFrame {
    /// Recording timestamp (ms) from the injected `Clock`.
    pub at_ms: i64,
    /// Global recording-order sequence drawn from the shared capture sequencer
    /// at the instant the frame was recorded. `0` when no shared sequencer was
    /// supplied (a standalone repl recording). When the failover harness threads
    /// the SIP recorder's sequencer in, this orders the frame against SIP
    /// messages and lifecycle markers in TRUE append order — `at_ms` is then only
    /// a display label, never the cross-source sort key (so a reboot marker
    /// appended just before the bootstrap pull it triggers sorts first even
    /// though both land on the same millisecond under the paused clock).
    pub seq: u64,
    /// Local endpoint of the connection that observed the frame.
    pub from: SocketAddr,
    /// Remote endpoint of that connection.
    pub to: SocketAddr,
    /// Sent vs. received.
    pub dir: Direction,
    /// The decoded frame.
    pub frame: Frame,
}

/// Shared capture sink — every wrapped connection appends here.
type Sink = Arc<Mutex<Vec<CapturedFrame>>>;

/// A shared global recording-order sequence source. The failover harness passes
/// an adapter over the SIP layer-harness `EventSequencer` so frames, SIP
/// messages, and lifecycle markers all draw from ONE counter and interleave in
/// true append order; a standalone repl recording leaves it `None` (frames get
/// `seq = 0` and fall back to capture-append ordering).
pub type CaptureSeq = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Records every frame that flows through the wrapped network. Clone shares the
/// same capture sink, so a clone kept for `captured()` sees all connections'
/// frames.
#[derive(Clone)]
pub struct RecordingReplicationNetwork {
    inner: Arc<dyn ReplicationNetwork>,
    clock: Clock,
    sink: Sink,
    /// Shared global recording-order sequence source (see [`CaptureSeq`]).
    seq: Option<CaptureSeq>,
}

impl RecordingReplicationNetwork {
    /// Wrap `inner`, stamping captures with `clock`. No shared sequence source:
    /// captured frames carry `seq = 0` and order purely by append.
    pub fn new(inner: Arc<dyn ReplicationNetwork>, clock: Clock) -> Self {
        Self {
            inner,
            clock,
            sink: Arc::new(Mutex::new(Vec::new())),
            seq: None,
        }
    }

    /// Wrap `inner`, stamping each captured frame with `clock` AND a global
    /// recording-order `seq` drawn from `seq` at the moment of capture. Use this
    /// to interleave repl frames with another plane's events (SIP / lifecycle)
    /// in true append order off ONE shared counter.
    pub fn with_seq(inner: Arc<dyn ReplicationNetwork>, clock: Clock, seq: CaptureSeq) -> Self {
        Self {
            inner,
            clock,
            sink: Arc::new(Mutex::new(Vec::new())),
            seq: Some(seq),
        }
    }

    /// Snapshot of every captured frame so far (in append order).
    pub fn captured(&self) -> Vec<CapturedFrame> {
        self.sink.lock().unwrap().clone()
    }

    /// A `CaptureSeq` backed by a fresh local atomic — for tests/standalone repl
    /// recordings that want a self-consistent capture order without a shared SIP
    /// sequencer. Starts at 1 (matching the layer-harness `EventSequencer`).
    pub fn local_seq() -> CaptureSeq {
        let ctr = Arc::new(AtomicU64::new(0));
        Arc::new(move || ctr.fetch_add(1, Ordering::Relaxed) + 1)
    }
}

#[async_trait]
impl ReplicationNetwork for RecordingReplicationNetwork {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let conn = self.inner.connect(dst).await?;
        Ok(Box::new(RecordingConnection {
            inner: conn,
            clock: self.clock.clone(),
            sink: self.sink.clone(),
            seq: self.seq.clone(),
        }))
    }

    async fn listen(
        &self,
        local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, ListenError> {
        let listener = self.inner.listen(local).await?;
        Ok(Box::new(RecordingListener {
            inner: listener,
            clock: self.clock.clone(),
            sink: self.sink.clone(),
            seq: self.seq.clone(),
        }))
    }
}

struct RecordingListener {
    inner: Box<dyn ReplicationListener>,
    clock: Clock,
    sink: Sink,
    seq: Option<CaptureSeq>,
}

#[async_trait]
impl ReplicationListener for RecordingListener {
    async fn accept(&self) -> Option<Box<dyn ReplicationConnection>> {
        let conn = self.inner.accept().await?;
        Some(Box::new(RecordingConnection {
            inner: conn,
            clock: self.clock.clone(),
            sink: self.sink.clone(),
            seq: self.seq.clone(),
        }))
    }

    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}

struct RecordingConnection {
    inner: Box<dyn ReplicationConnection>,
    clock: Clock,
    sink: Sink,
    seq: Option<CaptureSeq>,
}

impl RecordingConnection {
    fn capture(&self, dir: Direction, frame: &Frame) {
        // Draw the global recording-order seq INSIDE the sink lock so the `seq`
        // and the append position are consistent under concurrent connections.
        let mut sink = self.sink.lock().unwrap();
        let seq = self.seq.as_ref().map(|s| s()).unwrap_or(0);
        sink.push(CapturedFrame {
            at_ms: self.clock.now_ms(),
            seq,
            from: self.inner.local_addr(),
            to: self.inner.peer_addr(),
            dir,
            frame: frame.clone(),
        });
    }
}

#[async_trait]
impl ReplicationConnection for RecordingConnection {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        // Capture the decoded frame on the way out (the codec still plays in the
        // wrapped impl; we record the typed value the report renders).
        self.capture(Direction::Sent, &frame);
        self.inner.send(frame).await
    }

    async fn recv(&self) -> Option<Frame> {
        let frame = self.inner.recv().await?;
        self.capture(Direction::Received, &frame);
        Some(frame)
    }

    fn peer_addr(&self) -> SocketAddr {
        self.inner.peer_addr()
    }

    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;
    use std::time::Duration;

    use sip_clock::testkit::advance_in_100ms_chunks;

    use crate::transport::SimulatedReplicationNetwork;

    fn addr(port: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], port))
    }

    #[tokio::test(start_paused = true)]
    async fn captures_decoded_frames_with_direction_and_endpoints() {
        let sim = StdArc::new(SimulatedReplicationNetwork::with_delay(5));
        let rec = RecordingReplicationNetwork::new(sim, Clock::test_at(1_000_000));

        let listener = rec.listen(addr(2100)).await.unwrap();
        let client = rec.connect(addr(2100)).await.unwrap();
        let server = listener.accept().await.unwrap();

        let req = Frame::PullRequest {
            proto_ver: 1,
            caller: "node-A".into(),
            mode: crate::PullMode::Replog,
            since: crate::Watermark::new(1, 0),
            chunk: 128,
        };
        let data = Frame::Data {
            at: crate::Watermark::new(1, 1),
            op: crate::Op::Create,
            partition: crate::Partition::Bak,
            call_ref: "p|c1|t".into(),
            call_gen: 1,
            body_ttl_ms: 5000,
            indexes: vec!["i".into()],
            body: Some(StdArc::from(&b"body"[..])),
        };

        client.send(req.clone()).await.unwrap();
        advance_in_100ms_chunks(Duration::from_millis(10)).await;
        assert_eq!(server.recv().await, Some(req.clone()));

        server.send(data.clone()).await.unwrap();
        advance_in_100ms_chunks(Duration::from_millis(10)).await;
        assert_eq!(client.recv().await, Some(data.clone()));

        let cap = rec.captured();
        assert_eq!(cap.len(), 4, "expected 4 captured frames, got {}", cap.len());

        // client sent the PullRequest from its local addr to 2100.
        assert_eq!(cap[0].dir, Direction::Sent);
        assert_eq!(cap[0].to, addr(2100));
        assert_eq!(cap[0].frame, req);
        assert_eq!(cap[0].at_ms, 1_000_000);

        // server received it.
        assert_eq!(cap[1].dir, Direction::Received);
        assert_eq!(cap[1].from, addr(2100));
        assert_eq!(cap[1].frame, req);

        // server sent the Data.
        assert_eq!(cap[2].dir, Direction::Sent);
        assert_eq!(cap[2].from, addr(2100));
        assert_eq!(cap[2].frame, data);

        // client received it.
        assert_eq!(cap[3].dir, Direction::Received);
        assert_eq!(cap[3].to, addr(2100));
        assert_eq!(cap[3].frame, data);
    }
}
