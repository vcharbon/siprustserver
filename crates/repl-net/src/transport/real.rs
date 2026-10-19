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
    AcceptError, ConnectError, ListenError, ReplicationConnection, ReplicationListener,
    ReplicationNetwork, SendError, SocketSettings,
};

/// The real tokio-TCP replication network. Every stream it opens or accepts
/// carries its [`SocketSettings`]; clone is cheap.
#[derive(Clone, Default)]
pub struct RealReplicationNetwork {
    socket: SocketSettings,
}

impl RealReplicationNetwork {
    /// The real network with the default [`SocketSettings`].
    pub fn new() -> Self {
        Self::default()
    }

    /// The real network setting `socket` on every stream.
    pub fn with_socket(socket: SocketSettings) -> Self {
        Self { socket }
    }

    /// Set this network's socket options on a stream it opened or accepted;
    /// an option the kernel refuses is logged, the stream kept.
    fn tune(&self, stream: &TcpStream) {
        if let Err(error) = self.socket.apply(stream) {
            tracing::warn!(%error, "replication socket option refused");
        }
    }

    /// Open a tuned stream to `dst`.
    async fn open(&self, dst: SocketAddr) -> Result<TcpStream, ConnectError> {
        let stream = TcpStream::connect(dst).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::ConnectionRefused {
                ConnectError::Refused(dst)
            } else {
                ConnectError::Io(e.to_string())
            }
        })?;
        self.tune(&stream);
        Ok(stream)
    }

    /// Bind a listener at `local` whose accepted streams this network tunes.
    async fn bind(&self, local: SocketAddr) -> Result<RealListener, ListenError> {
        let listener = TcpListener::bind(local).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::AddrInUse {
                ListenError::AlreadyInUse(local)
            } else {
                ListenError::Io(e.to_string())
            }
        })?;
        let local_addr = listener.local_addr().map_err(|e| ListenError::Io(e.to_string()))?;
        Ok(RealListener { inner: listener, local_addr, network: self.clone() })
    }
}

#[async_trait]
impl ReplicationNetwork for RealReplicationNetwork {
    async fn connect(
        &self,
        dst: SocketAddr,
    ) -> Result<Box<dyn ReplicationConnection>, ConnectError> {
        let stream = self.open(dst).await?;
        RealConnection::from_stream(stream).map_err(ConnectError::Io)
    }

    async fn listen(&self, local: SocketAddr) -> Result<Box<dyn ReplicationListener>, ListenError> {
        Ok(Box::new(self.bind(local).await?))
    }
}

struct RealListener {
    inner: TcpListener,
    local_addr: SocketAddr,
    /// The network whose socket options every accepted stream takes.
    network: RealReplicationNetwork,
}

impl RealListener {
    /// Accept the next stream, tuned. An error is one failed accept: the
    /// listener keeps accepting.
    async fn take(&self) -> std::io::Result<TcpStream> {
        let (stream, _peer) = self.inner.accept().await?;
        self.network.tune(&stream);
        Ok(stream)
    }
}

#[async_trait]
impl ReplicationListener for RealListener {
    async fn accept(&self) -> Result<Box<dyn ReplicationConnection>, AcceptError> {
        let stream = self.take().await.map_err(|e| AcceptError::Io(e.to_string()))?;
        RealConnection::from_stream(stream).map_err(AcceptError::Io)
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
        let (read_half, write_half) = stream.into_split();
        Ok(Box::new(RealConnection {
            peer,
            local,
            write: Mutex::new(write_half),
            read: Mutex::new(ReadState { half: read_half, buf: Vec::new(), done: false }),
        }))
    }
}

#[async_trait]
impl ReplicationConnection for RealConnection {
    async fn send(&self, frame: Frame) -> Result<(), SendError> {
        // Codec + length prefix, then one write.
        let wire = wire_bytes(&frame)?;
        let mut w = self.write.lock().await;
        w.write_all(&wire).await.map_err(|e| classify_write(&e))?;
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
            buf.extend_from_slice(&wire_bytes(frame)?);
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
                        // `read` reports at most the chunk's length.
                        Ok(n) => rs.buf.extend_from_slice(chunk.get(..n).unwrap_or(&chunk)),
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

/// `frame` encoded and length-prefixed; one that does not encode or frame is
/// [`SendError::Unencodable`], and nothing of it is written.
fn wire_bytes(frame: &Frame) -> Result<Vec<u8>, SendError> {
    let payload = encode_frame(frame).map_err(|e| SendError::Unencodable(e.to_string()))?;
    frame_with_len_prefix(&payload).map_err(|e| SendError::Unencodable(e.to_string()))
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
        let noop = Frame::Noop { at: crate::Watermark::new(3, 9) };
        let data = Frame::Data {
            at: crate::Watermark::new(3, 10),
            op: crate::Op::Put,
            partition: crate::Partition::Pri,
            call_ref: "primary|callXYZ|fromTag".into(),
            call_gen: 42,
            call_bgen: 0,
            body_ttl_ms: 30_000,
            origin_now_ms: 0,
            indexes: vec!["a".into(), "b".into()],
            body: Some(StdArc::from(&b"the-encoded-call-body-bytes"[..])),
            answered: false,
            incarnation: None,
        };

        client.send(noop.clone()).await.unwrap();
        client.send(data.clone()).await.unwrap();

        assert_eq!(server.recv().await, Some(noop));
        assert_eq!(server.recv().await, Some(data));

        // Bidirectional.
        let reset = Frame::ResetToBootstrap { reason: "tail fell off".into() };
        server.send(reset.clone()).await.unwrap();
        assert_eq!(client.recv().await, Some(reset));

        // Clean close: drop the client; server recv yields None.
        drop(client);
        assert_eq!(server.recv().await, None);
    }

    /// Both ends of a replication stream carry TCP keepalive and the user
    /// timeout as configured, so a peer that vanished without a reset is
    /// detected by the kernel on an idle and on a busy socket alike. The
    /// streams are the ones `connect` and `accept` wrap.
    #[tokio::test]
    async fn both_ends_carry_keepalive_and_a_user_timeout() {
        use std::time::Duration;
        let settings = SocketSettings {
            keepalive_idle: Duration::from_secs(7),
            keepalive_interval: Duration::from_secs(3),
            keepalive_retries: 4,
            user_timeout: Duration::from_millis(12_500),
        };
        let net = RealReplicationNetwork::with_socket(settings);
        let listener = net.bind(loopback()).await.unwrap();
        let (client, server) = tokio::join!(net.open(listener.local_addr), listener.take());
        let (client, server) = (client.unwrap(), server.unwrap());
        for (end, stream) in [("client", &client), ("server", &server)] {
            let sock = socket2::SockRef::from(stream);
            assert!(sock.keepalive().unwrap(), "{end}: SO_KEEPALIVE");
            assert_eq!(sock.tcp_keepalive_time().unwrap(), Duration::from_secs(7), "{end}");
            assert_eq!(sock.tcp_keepalive_interval().unwrap(), Duration::from_secs(3), "{end}");
            assert_eq!(sock.tcp_keepalive_retries().unwrap(), 4, "{end}");
            assert_eq!(
                sock.tcp_user_timeout().unwrap(),
                Some(Duration::from_millis(12_500)),
                "{end}: TCP_USER_TIMEOUT"
            );
            assert!(sock.tcp_nodelay().unwrap(), "{end}: TCP_NODELAY");
        }
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
