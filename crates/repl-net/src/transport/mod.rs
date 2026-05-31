//! The transport seam (slice S3): a connection-oriented, reliable, ordered,
//! message-granular replication network.
//!
//! This is the replication analogue of `sip-net`'s `SignalingNetwork`, but for
//! a **reliable framed stream** (Decision X2). Where SIP rides fire-and-forget
//! UDP datagrams, replication rides a connection that delivers whole encoded
//! [`crate::Frame`]s in order with no loss until the connection is cut — so it
//! gets its own seam rather than a reliability layer bolted onto the UDP sim.
//!
//! ## The codec plays through every path
//! `send` always [`encode_frame`](crate::encode_frame)s to bytes and moves the
//! bytes; `recv` always [`decode_frame`](crate::decode_frame)s back. Even the
//! in-process [`SimulatedReplicationNetwork`] never passes a `Frame` by
//! reference — it moves an encoded `Vec<u8>` through its channels — so the wire
//! codec is exercised in **every** test path (Decision X2), and the recording
//! layer can decode + display each replication message.
//!
//! ## Three impls
//! - [`SimulatedReplicationNetwork`] — in-process ordered delivery, the
//!   workhorse for the fake-clock tests (goals 1–2). Mandatory, not a
//!   convenience: real `TcpStream` readiness does **not** obey
//!   `tokio::time::pause`, so the paused-clock scenarios *cannot* use real TCP.
//! - [`RealReplicationNetwork`] — tokio TCP + 4-byte length prefix; goal-3
//!   only, and its tests run on a **real** (non-paused) runtime.
//! - [`RecordingReplicationNetwork`] — a decorator that tees every decoded
//!   frame into a capture sink for the report.

mod real;
mod recording;
mod simulated;

use std::net::SocketAddr;

use async_trait::async_trait;

use crate::Frame;

pub use real::RealReplicationNetwork;
pub use recording::{CapturedFrame, Direction, RecordingReplicationNetwork};
pub use simulated::{Fault, SimulatedReplicationNetwork};

/// Failure opening an outbound connection.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ConnectError {
    /// No listener is bound at the destination (sim: nothing in the routing
    /// table; real: connection refused).
    #[error("connection refused: no listener at {0}")]
    Refused(SocketAddr),
    /// The destination is reachable but a fault (partition/cut) is blocking the
    /// pair right now.
    #[error("connection blocked to {addr}: {reason}")]
    Blocked {
        /// The destination that could not be reached.
        addr: SocketAddr,
        /// Human-readable cause (for logs/recording).
        reason: String,
    },
    /// Underlying I/O failure (real transport only).
    #[error("connect io error: {0}")]
    Io(String),
}

/// Failure binding a listener.
#[derive(Debug, Clone, thiserror::Error)]
pub enum ListenError {
    /// Another listener already owns this local address.
    #[error("address already in use: {0}")]
    AlreadyInUse(SocketAddr),
    /// Underlying I/O failure (real transport only).
    #[error("listen io error: {0}")]
    Io(String),
}

/// Failure sending a frame on a connection.
#[derive(Debug, Clone, thiserror::Error)]
pub enum SendError {
    /// The connection has been cut (peer crash, partition, or buffer-overflow
    /// drop) — no further frames can be sent.
    #[error("connection closed")]
    Closed,
    /// Underlying I/O failure (real transport only).
    #[error("send io error: {0}")]
    Io(String),
}

/// A connection-oriented, reliable, ordered, framed replication network.
///
/// Parallel to `sip-net::SignalingNetwork` but for a reliable stream. A node
/// [`listen`](ReplicationNetwork::listen)s on its local replication address and
/// [`connect`](ReplicationNetwork::connect)s out to peers; each side then sends
/// and receives whole [`Frame`]s over the returned [`ReplicationConnection`].
#[async_trait]
pub trait ReplicationNetwork: Send + Sync {
    /// Open a connection to a peer's listener. The returned connection is the
    /// **client** end; the peer obtains the matching **server** end from its
    /// listener's [`accept`](ReplicationListener::accept).
    async fn connect(&self, dst: SocketAddr) -> Result<Box<dyn ReplicationConnection>, ConnectError>;

    /// Bind a listener at `local`. Dropping it stops accepting new connections.
    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError>;
}

/// The accepting side of a bound listener.
#[async_trait]
pub trait ReplicationListener: Send + Sync {
    /// Await the next inbound connection (the server end). `None` once the
    /// listener is closed and drained.
    async fn accept(&self) -> Option<Box<dyn ReplicationConnection>>;

    /// The bound local address.
    fn local_addr(&self) -> SocketAddr;
}

/// One end of a reliable, ordered, framed replication connection.
///
/// **The codec plays through every path**: [`send`](ReplicationConnection::send)
/// encodes the frame to bytes and moves the bytes;
/// [`recv`](ReplicationConnection::recv) decodes bytes back into a frame.
#[async_trait]
pub trait ReplicationConnection: Send + Sync {
    /// Send one frame. Returns when the frame has been accepted into the
    /// outbound path (which, under backpressure, may await buffer space).
    /// `Err(SendError::Closed)` once the connection is cut.
    async fn send(&self, frame: Frame) -> Result<(), SendError>;

    /// Await the next inbound frame. `None` on a clean close / cut.
    async fn recv(&self) -> Option<Frame>;

    /// The remote address of this connection.
    fn peer_addr(&self) -> SocketAddr;

    /// The local address of this connection.
    fn local_addr(&self) -> SocketAddr;
}
