//! `RealReplicationNetwork` — tokio TCP + 4-byte length prefix. **Goal-3 only.**
//!
//! Real `TcpStream` I/O readiness does **not** obey `tokio::time::pause`
//! (Decision X2), so this impl cannot run under the fake-clock scenarios — its
//! tests use a **real** (non-paused) runtime and bind `127.0.0.1:0`. The sim
//! transport is mandatory for goals 1–2; this is the kind/k8s path.
//!
//! Wire: `send` = `frame_with_len_prefix(encode_frame(f))` written to the
//! socket; the read side accumulates socket bytes into a buffer and pops whole
//! frames with `try_read_framed` then `decode_frame`. `MAX_FRAME_LEN` is
//! honoured (an oversized prefix cuts the connection). Clean EOF → `recv` None.

use std::net::SocketAddr;

use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;

use crate::{decode_frame, encode_frame, frame_with_len_prefix, try_read_framed, Frame};

use super::{
    ConnectError, ListenError, ReplicationConnection, ReplicationListener, ReplicationNetwork,
    SendError,
};

/// The real tokio-TCP replication network. Stateless; clone is cheap.
#[derive(Clone, Default)]
pub struct RealReplicationNetwork;

impl RealReplicationNetwork {
    /// Construct the real network.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl ReplicationNetwork for RealReplicationNetwork {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let stream = TcpStream::connect(dst).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                ConnectError::Refused(dst)
            } else {
                ConnectError::Io(e.to_string())
            }
        })?;
        RealConnection::from_stream(stream).map_err(ConnectError::Io)
    }

    async fn listen(
        &self,
        local: SocketAddr,
    ) -> Result<Box<dyn ReplicationListener>, ListenError> {
        let listener = TcpListener::bind(local).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                ListenError::AlreadyInUse(local)
            } else {
                ListenError::Io(e.to_string())
            }
        })?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ListenError::Io(e.to_string()))?;
        Ok(Box::new(RealListener {
            inner: listener,
            local_addr,
        }))
    }
}

struct RealListener {
    inner: TcpListener,
    local_addr: SocketAddr,
}

#[async_trait]
impl ReplicationListener for RealListener {
    async fn accept(&self) -> Option<Box<dyn ReplicationConnection>> {
        let (stream, _peer) = self.inner.accept().await.ok()?;
        RealConnection::from_stream(stream).ok()
    }

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }
}

/// A connection over a TCP stream. The stream is split into an owned write half
/// (behind a mutex for `send`) and read half (behind a mutex for `recv`), each
/// independently `await`able.
struct RealConnection {
    peer: SocketAddr,
    local: SocketAddr,
    write: Mutex<tokio::net::tcp::OwnedWriteHalf>,
    read: Mutex<ReadState>,
}

struct ReadState {
    half: tokio::net::tcp::OwnedReadHalf,
    /// Accumulated bytes not yet split into whole frames.
    buf: Vec<u8>,
    /// Once a desync/oversize/EOF is seen, recv returns None forever.
    done: bool,
}

impl RealConnection {
    fn from_stream(stream: TcpStream) -> Result<Box<dyn ReplicationConnection>, String> {
        let peer = stream.peer_addr().map_err(|e| e.to_string())?;
        let local = stream.local_addr().map_err(|e| e.to_string())?;
        let _ = stream.set_nodelay(true);
        let (read_half, write_half) = stream.into_split();
        Ok(Box::new(RealConnection {
            peer,
            local,
            write: Mutex::new(write_half),
            read: Mutex::new(ReadState {
                half: read_half,
                buf: Vec::new(),
                done: false,
            }),
        }))
    }
}

#[async_trait]
impl ReplicationConnection for RealConnection {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        // Codec + length prefix, then one write.
        let wire = frame_with_len_prefix(&encode_frame(&frame));
        let mut w = self.write.lock().await;
        w.write_all(&wire)
            .await
            .map_err(|e| classify_write(&e))?;
        w.flush().await.map_err(|e| classify_write(&e))?;
        Ok(())
    }

    async fn send_batch(&self, frames: Vec<Frame>) -> Result<(), SendError> {
        if frames.is_empty() {
            return Ok(());
        }
        // Encode every frame (each with its own length prefix) into ONE buffer,
        // then a SINGLE write + flush — so a 128-body bootstrap chunk costs one
        // syscall / one flush / one write-lock acquisition instead of 128. The
        // bytes on the wire are byte-for-byte identical to sending them
        // individually, so the receiver's framed `recv` is unchanged.
        let mut buf = Vec::new();
        for frame in &frames {
            buf.extend_from_slice(&frame_with_len_prefix(&encode_frame(frame)));
        }
        let mut w = self.write.lock().await;
        w.write_all(&buf).await.map_err(|e| classify_write(&e))?;
        w.flush().await.map_err(|e| classify_write(&e))?;
        Ok(())
    }

    async fn recv(&self) -> Option<Frame> {
        let mut rs = self.read.lock().await;
        loop {
            if rs.done {
                return None;
            }
            // Try to pop a complete frame from what we already have.
            match try_read_framed(&mut rs.buf) {
                Ok(Some(payload)) => match decode_frame(&payload) {
                    Ok(frame) => return Some(frame),
                    Err(_) => {
                        // Malformed frame on a "reliable" stream → desync; cut.
                        rs.done = true;
                        return None;
                    }
                },
                Ok(None) => {
                    // Need more bytes from the socket.
                    let mut chunk = [0u8; 16 * 1024];
                    match rs.half.read(&mut chunk).await {
                        Ok(0) => {
                            // Clean EOF.
                            rs.done = true;
                            return None;
                        }
                        Ok(n) => rs.buf.extend_from_slice(&chunk[..n]),
                        Err(_) => {
                            rs.done = true;
                            return None;
                        }
                    }
                }
                Err(_oversized) => {
                    // Length prefix exceeds MAX_FRAME_LEN → desync/hostile; cut.
                    rs.done = true;
                    return None;
                }
            }
        }
    }

    fn peer_addr(&self) -> SocketAddr {
        self.peer
    }

    fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

fn classify_write(e: &std::io::Error) -> SendError {
    use std::io::ErrorKind::*;
    match e.kind() {
        BrokenPipe | ConnectionReset | ConnectionAborted | NotConnected => SendError::Closed,
        _ => SendError::Io(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc as StdArc;

    fn loopback() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 0))
    }

    // REAL runtime — NOT start_paused. Real TcpStream readiness ignores
    // tokio::time::pause (Decision X2).
    #[tokio::test]
    async fn real_tcp_loopback_roundtrip_and_clean_close() {
        let net = RealReplicationNetwork::new();
        let listener = net.listen(loopback()).await.unwrap();
        let server_addr = listener.local_addr();

        // Accept concurrently with connect.
        let accept_task = tokio::spawn(async move { listener.accept().await });

        let client = net.connect(server_addr).await.unwrap();
        let server = accept_task.await.unwrap().expect("accepted");

        // A small frame and a Data frame with a body, both round-trip identically.
        let noop = Frame::Noop {
            at: crate::Watermark::new(3, 9),
        };
        let data = Frame::Data {
            at: crate::Watermark::new(3, 10),
            op: crate::Op::Update,
            partition: crate::Partition::Pri,
            call_ref: "primary|callXYZ|fromTag".into(),
            call_gen: 42,
            call_bgen: 0,
            body_ttl_ms: 30_000,
            indexes: vec!["a".into(), "b".into()],
            body: Some(StdArc::from(&b"the-encoded-call-body-bytes"[..])),
        };

        client.send(noop.clone()).await.unwrap();
        client.send(data.clone()).await.unwrap();

        assert_eq!(server.recv().await, Some(noop));
        assert_eq!(server.recv().await, Some(data));

        // Bidirectional.
        let reset = Frame::ResetToBootstrap {
            reason: "tail fell off".into(),
        };
        server.send(reset.clone()).await.unwrap();
        assert_eq!(client.recv().await, Some(reset));

        // Clean close: drop the client; server recv yields None.
        drop(client);
        assert_eq!(server.recv().await, None);
    }

    #[tokio::test]
    async fn real_tcp_connect_refused() {
        let net = RealReplicationNetwork::new();
        // Bind+drop to obtain an almost-certainly-free port, then connect.
        let l = net.listen(loopback()).await.unwrap();
        let addr = l.local_addr();
        drop(l);
        // Give the OS a moment to release; if still bound the test is lenient
        // (refused or io error both acceptable as "not connected").
        let r = net.connect(addr).await;
        assert!(r.is_err(), "connect to unbound port should fail");
    }
}
