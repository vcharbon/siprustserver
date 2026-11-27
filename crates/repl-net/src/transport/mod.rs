//! The transport seam: a connection-oriented, reliable, ordered,
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
//! in-process simulated network never passes a `Frame` by reference — it moves
//! an encoded `Vec<u8>` through its channels — so the wire codec is exercised
//! in **every** test path (Decision X2), and the recording layer can decode +
//! display each replication message.
//!
//! ## The production impl
//! [`RealReplicationNetwork`] — tokio TCP + 4-byte length prefix; its tests run
//! on a **real** (non-paused) runtime. The in-process simulated network (the
//! workhorse of the fake-clock tests: real `TcpStream` readiness does **not**
//! obey `tokio::time::pause`) and the recording decorator implement the same
//! seam in `repl-net-testkit`.

mod real;
mod socket_settings;

use std::net::SocketAddr;

use async_trait::async_trait;

use crate::framing::MAX_FRAME_LEN;
use crate::Frame;

pub use real::RealReplicationNetwork;
pub use socket_settings::SocketSettings;

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

/// Failure accepting an inbound connection.
#[derive(Debug, Clone, thiserror::Error)]
pub enum AcceptError {
    /// The listener is closed: it accepts nothing more.
    #[error("listener closed")]
    Closed,
    /// One accept failed (descriptors exhausted, a handshake aborted, …). The
    /// listener stays open: the caller accepts again.
    #[error("accept io error: {0}")]
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
    /// The frame, or a frame of a batch, does not encode or exceeds the frame
    /// cap ([`crate::framing::MAX_FRAME_LEN`]): nothing of the frame or of the
    /// batch was sent, and the connection stays usable.
    #[error("frame not sent: {0}")]
    Unencodable(String),
}

/// `frame` encoded, within the frame cap; one that does not encode or passes
/// [`MAX_FRAME_LEN`] is [`SendError::Unencodable`].
pub fn payload(frame: &Frame) -> Result<Vec<u8>, SendError> {
    let payload = crate::encode_frame(frame).map_err(|e| SendError::Unencodable(e.to_string()))?;
    if u32::try_from(payload.len()).map_or(true, |len| len > MAX_FRAME_LEN) {
        let len = payload.len();
        return Err(SendError::Unencodable(format!("{len} bytes past the {MAX_FRAME_LEN} cap")));
    }
    Ok(payload)
}

/// `frame` encoded and length-prefixed, as a connection writes it; one that
/// does not encode or frame is [`SendError::Unencodable`].
pub fn wire_bytes(frame: &Frame) -> Result<Vec<u8>, SendError> {
    let mut wire = Vec::new();
    append_wire_bytes(frame, &mut wire)?;
    Ok(wire)
}

/// Append [`wire_bytes`] of `frame` to `out`, encoded in place behind its
/// length prefix: a batch of frames is one buffer, never a buffer per frame
/// beside it. On an error `out` is left as it was.
pub fn append_wire_bytes(frame: &Frame, out: &mut Vec<u8>) -> Result<(), SendError> {
    let start = out.len();
    out.extend_from_slice(&[0; 4]);
    if let Err(e) = crate::codec::encode_frame_onto(frame, out) {
        out.truncate(start);
        return Err(SendError::Unencodable(e.to_string()));
    }
    let len = out.len().saturating_sub(start).saturating_sub(4);
    let Some(prefix) = u32::try_from(len).ok().filter(|len| *len <= MAX_FRAME_LEN) else {
        out.truncate(start);
        return Err(SendError::Unencodable(format!("{len} bytes past the {MAX_FRAME_LEN} cap")));
    };
    if let Some(slot) = out.get_mut(start..).and_then(<[u8]>::first_chunk_mut::<4>) {
        *slot = prefix.to_be_bytes();
    }
    Ok(())
}

/// The fixed part of a `Data` frame's [`wire_bytes`], above its 84 bytes at
/// msgpack's widest headers: the length prefix, the integers and the headers
/// of its four variable-length fields.
const DATA_FIXED_WIRE_BYTES: usize = 128;

/// The bytes a buffer reserves for [`wire_bytes`] of `frame`: at least them
/// for a `Data` frame, whose indexes each add a header of at most 5 bytes;
/// the 128 bytes of a `Data` frame's fixed part for the other frames, which
/// carry no body.
pub fn wire_len_bound(frame: &Frame) -> usize {
    match frame {
        Frame::Data { call_ref, indexes, body, incarnation, .. } => {
            let indexes: usize = indexes.iter().map(|idx| idx.len().saturating_add(5)).sum();
            DATA_FIXED_WIRE_BYTES
                .saturating_add(call_ref.len())
                .saturating_add(indexes)
                .saturating_add(body.as_ref().map_or(0, |body| body.len()))
                .saturating_add(incarnation.as_ref().map_or(0, String::len))
        }
        _ => DATA_FIXED_WIRE_BYTES,
    }
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
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError>;

    /// Bind a listener at `local`. Dropping it stops accepting new connections.
    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError>;
}

/// The accepting side of a bound listener.
#[async_trait]
pub trait ReplicationListener: Send + Sync {
    /// Await the next inbound connection (the server end).
    /// `Err(AcceptError::Closed)` once the listener is closed and drained;
    /// `Err(AcceptError::Io)` for one failed accept, after which the listener
    /// still accepts.
    async fn accept(&self) -> Result<Box<dyn ReplicationConnection>, AcceptError>;

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

    /// Send several frames as ONE unit: encode them all, then a single write +
    /// flush. The **wire is unchanged** — each frame keeps its own length
    /// prefix, so any receiver decodes the batch exactly as it would the same
    /// frames sent one-by-one; this only amortises the per-frame write/flush
    /// syscall (and, on a real socket, the per-segment network overhead and the
    /// serialised write-lock acquisition) across the whole batch. The default
    /// loops [`send`](Self::send) (correct, unoptimised); the real TCP transport
    /// overrides it to coalesce. Used by the bulk bootstrap scan, where
    /// hundreds–thousands of small bodies stream back-to-back and a per-frame
    /// flush collapses throughput on any non-loopback link. A batch holding a
    /// frame that does not encode is [`SendError::Unencodable`] and nothing of
    /// it is sent: the default checks every frame before the first send. The
    /// batch's encoded bytes are held once, in the one buffer the write takes.
    async fn send_batch(&self, frames: Vec<Frame>) -> Result<(), SendError> {
        for frame in &frames {
            payload(frame)?;
        }
        for frame in frames {
            self.send(frame).await?;
        }
        Ok(())
    }

    /// Await the next inbound frame. `None` on a clean close / cut.
    async fn recv(&self) -> Option<Frame>;

    /// The remote address of this connection.
    fn peer_addr(&self) -> SocketAddr;

    /// The local address of this connection.
    fn local_addr(&self) -> SocketAddr;
}
