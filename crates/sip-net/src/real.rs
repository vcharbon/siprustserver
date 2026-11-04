//! Real `dgram`-backed `SignalingNetwork` (port of `SignalingNetwork.real.ts`),
//! over `tokio::net::UdpSocket`.
//!
//! Each `bind_udp` opens a real UDP socket and spawns a receive task that
//! pumps `recv_from` into the endpoint's bounded [`PacketQueue`], applying the
//! pre-ingress hook at arrival time exactly as the source's `socket.on(
//! "message")` handler did. Every send is one non-blocking `sendto`: a full
//! send buffer drops the datagram and counts it, it never suspends the caller
//! (ADR-0033 — the kernel can hold a socket's send buffer for seconds on an
//! unresolved next hop, and the caller is an ingress loop). Trace recording is NOT here — in this port the
//! typed `Recorder` channel (the recording decorator in `contracts.rs`) is the
//! single recording path, replacing the source's `realTracing` boolean split.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sip_clock::Clock;
use sip_message::Bytes;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::arrival::Arrival;
use crate::fragmentation::pin_fragmentation;
use crate::net::{Counters, SignalingNetwork, UdpEndpoint};
use crate::pre_ingress;
use crate::queue::PacketQueue;
use crate::socket_stats::{rx_dropped, socket_buffers};
use crate::types::{
    BindError, BindErrorReason, BindUdpOpts, PreIngressAction, PreIngressHook, SendError,
    SocketBuffers, UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};

/// Build the UDP socket every real bind stands on. socket2, always, because
/// two things this stack needs have no tokio knob: the path-MTU-discovery mode
/// (ADR-0027 — signalling is UDP-only, so an oversize datagram MUST fragment at
/// IP rather than fail the send) and `SO_REUSEPORT`. N reuse-port sockets on
/// one port shard the recv path across N tasks; the kernel flow-hashes on the
/// 4-tuple, so all datagrams from one src:port land on ONE socket and per-flow
/// ordering (INVITE→CANCEL, retransmits) is preserved. `SO_SNDBUF` and
/// `SO_RCVBUF` are requested from `opts` (`None` keeps the kernel default).
/// Public so a test reads the options back off the very socket the bind path
/// produces.
pub fn build_bound_socket(opts: &BindUdpOpts) -> std::io::Result<socket2::Socket> {
    let addr = opts.addr;
    let domain = socket2::Domain::for_address(addr);
    let raw = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    if opts.reuse_port {
        raw.set_reuse_port(true)?;
    }
    if let Some(bytes) = opts.send_buffer_bytes {
        raw.set_send_buffer_size(bytes)?;
    }
    if let Some(bytes) = opts.recv_buffer_bytes {
        raw.set_recv_buffer_size(bytes)?;
    }
    pin_fragmentation(&raw, addr.is_ipv4())?;
    // tokio's reactor requires the fd non-blocking.
    raw.set_nonblocking(true)?;
    raw.bind(&addr.into())?;
    Ok(raw)
}

/// Production network. Stateless — every `bind_udp` is an independent socket.
#[derive(Debug, Default, Clone, Copy)]
pub struct RealSignalingNetwork;

impl RealSignalingNetwork {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl SignalingNetwork for RealSignalingNetwork {
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError> {
        let os_err = |e: std::io::Error| BindError {
            // EADDRINUSE is kept structurally distinguishable: it is the one
            // bind failure a caller can meaningfully wait out (a predecessor
            // process releasing the port).
            reason: if e.kind() == std::io::ErrorKind::AddrInUse {
                BindErrorReason::AddrInUse
            } else {
                BindErrorReason::OsError
            },
            addr: opts.addr,
            message: e.to_string(),
        };
        let raw = build_bound_socket(&opts).map_err(os_err)?;
        // `counters()` reads the kernel drop count on every snapshot; a socket
        // that cannot answer is refused here rather than reading 0 forever.
        rx_dropped(&raw).map_err(os_err)?;
        let socket = UdpSocket::from_std(raw.into()).map_err(os_err)?;
        let local = socket.local_addr().map_err(|e| BindError {
            reason: BindErrorReason::OsError,
            addr: opts.addr,
            message: e.to_string(),
        })?;

        let socket = Arc::new(socket);
        let queue = Arc::new(PacketQueue::new(opts.queue_max));
        let counters = Arc::new(Counters::default());

        let task = tokio::spawn(recv_loop(
            socket.clone(),
            queue.clone(),
            counters.clone(),
            opts.pre_ingress.clone(),
            opts.clock.clone(),
        ));

        Ok(Box::new(RealEndpoint {
            socket,
            queue,
            counters,
            local,
            queue_max: opts.queue_max,
            task,
        }))
    }

    async fn drain_undeliverable(&self) -> Vec<UndeliveredPacket> {
        Vec::new()
    }

    fn transit_delay_ms(&self) -> Option<u64> {
        None
    }

    fn in_flight(&self) -> i64 {
        0
    }

    fn bump_in_flight(&self, _delta: i64) {}

    fn queue_depths(&self) -> Vec<(SocketAddr, usize)> {
        // dgram sockets expose no structural queue snapshot; the layer-close
        // audit skips queue-leak checks for the real impl (transit_delay None).
        Vec::new()
    }

    async fn await_in_flight(&self, _timeout: Duration) {}
}

/// The receive buffer one `recv_from` reads into. Linux TRUNCATES a datagram
/// that does not fit and reports the truncated length, so a buffer smaller than
/// the largest datagram the socket can receive tears messages silently — this
/// inequality is the only thing standing between the kernel's reassembled
/// datagram and a torn SIP message (ADR-0027).
const RECV_BUF_LEN: usize = 65_536;
#[expect(clippy::disallowed_macros, reason = "checked at compile time")]
const _: () = assert!(RECV_BUF_LEN > crate::types::MAX_UDP_PAYLOAD);

/// The wait after a receive error that is not about one datagram, so a lasting
/// failure (memory exhausted) does not spin the pump. The kernel buffers what
/// arrives meanwhile.
const RECV_PAUSE: Duration = Duration::from_millis(10);

/// The pause before the next receive after `error`: none for an error that
/// one datagram or a signal caused (an ICMP refusal or unreachable, which the
/// kernel reports on a connected socket only; an interrupted call),
/// [`RECV_PAUSE`] for any other.
fn recv_pause(error: &std::io::Error) -> Option<Duration> {
    use std::io::ErrorKind::{
        ConnectionRefused, ConnectionReset, HostUnreachable, Interrupted, NetworkUnreachable,
    };
    match error.kind() {
        ConnectionRefused | ConnectionReset | HostUnreachable | NetworkUnreachable
        | Interrupted => None,
        _ => Some(RECV_PAUSE),
    }
}

/// The receive pump. Mirrors the source's `socket.on("message", ...)` handler:
/// depth-aware pre-ingress dispatch, tail-drop on a full queue. An accepted
/// datagram is queued with the parse its hook made, if any. A receive
/// error never ends it: each is counted ([`UdpEndpointCounters::recv_errors`])
/// and the pump reads on, after [`recv_pause`].
async fn recv_loop(
    socket: Arc<UdpSocket>,
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    pre_ingress: Option<PreIngressHook>,
    clock: Clock,
) {
    // However the pump ends — an abort, a panic — the queue closes, so the
    // endpoint's `recv()` resolves to None and the owner task can wind down. Without it the layer goes permanently DEAF — sends
    // still work, nothing is ever received, no panic/metric/readiness change —
    // because the endpoint is owned by the very owner task blocked on
    // `recv()`, so its `Drop` (the other `close` site) can never run until
    // that `recv()` returns.
    let _closes = queue.close_on_drop();
    let mut buf = vec![0u8; RECV_BUF_LEN];
    loop {
        let (n, src) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(error) => {
                counters.recv_errors.fetch_add(1, Ordering::Relaxed);
                if let Some(pause) = recv_pause(&error) {
                    tokio::time::sleep(pause).await;
                }
                continue;
            }
        };
        // `recv_from` reports at most the buffer's length.
        let mut arrival = Arrival::new(Bytes::copy_from_slice(buf.get(..n).unwrap_or(&buf)));
        let depth = queue.depth();
        match pre_ingress::judge(pre_ingress.as_ref(), &mut arrival, src, depth, &counters) {
            PreIngressAction::Drop => {
                counters.pre_ingress_dropped.fetch_add(1, Ordering::Relaxed);
            }
            PreIngressAction::Reply { datagram, to } => {
                counters.pre_ingress_replies.fetch_add(1, Ordering::Relaxed);
                // The reply is best-effort — the pump must keep receiving — and
                // a failure is COUNTED, never printed: this crate reports
                // through counters alone, and a peer that rejects every reply
                // would otherwise print once per datagram on the receive path.
                if send_now(&socket, &counters, &datagram, to).is_err() {
                    counters.pre_ingress_reply_failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            PreIngressAction::Accept => {
                let arrival_ms = u64::try_from(clock.now_ms()).unwrap_or(0);
                if queue.offer(arrival.into_packet(src, arrival_ms)) {
                    counters.enqueued.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.tail_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
}

/// The one send path of a real socket: one non-blocking `sendto` on the fd,
/// never a suspension. A full send buffer (`EAGAIN`) is a counted drop. The
/// kernel charges a datagram to the socket until the interface transmits it,
/// and a next hop it cannot resolve holds it for the whole ARP cycle — a
/// blocking send would park the caller for seconds per cycle, and every
/// caller is an ingress loop. The syscall is made directly rather than through
/// tokio's `try_send_to`: that one answers from the reactor's readiness cache,
/// which reads "not writable" on a fresh socket until the first poll.
fn send_now(
    socket: &UdpSocket,
    counters: &Counters,
    buf: &[u8],
    dst: SocketAddr,
) -> Result<(), SendError> {
    socket2::SockRef::from(socket).send_to(buf, &dst.into()).map(|_| ()).map_err(|e| {
        let err = SendError::from(e);
        if err.kind == crate::types::SendErrorKind::WouldBlock {
            counters.send_would_block.fetch_add(1, Ordering::Relaxed);
        }
        err
    })
}

struct RealEndpoint {
    socket: Arc<UdpSocket>,
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    local: SocketAddr,
    queue_max: usize,
    task: JoinHandle<()>,
}

#[async_trait]
impl UdpEndpoint for RealEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        send_now(&self.socket, &self.counters, buf, dst)
    }

    async fn recv(&self) -> Option<UdpPacket> {
        self.queue.take().await
    }

    fn try_recv(&self) -> Option<UdpPacket> {
        self.queue.poll()
    }

    fn local_addr(&self) -> SocketAddr {
        self.local
    }

    fn queue_depth(&self) -> usize {
        self.queue.depth()
    }

    fn queue_max(&self) -> usize {
        self.queue_max
    }

    fn counters(&self) -> UdpEndpointCounters {
        UdpEndpointCounters {
            // The bind proved the socket answers `SO_MEMINFO`.
            kernel_rx_dropped: rx_dropped(&*self.socket).unwrap_or(0),
            ..self.counters.snapshot()
        }
    }

    fn socket_buffers(&self) -> Option<SocketBuffers> {
        socket_buffers(&*self.socket).ok()
    }

    fn install_recv_tap(&self, tap: crate::types::RecvTap) -> bool {
        self.queue.install_tap(tap);
        true
    }
}

impl Drop for RealEndpoint {
    fn drop(&mut self) {
        self.task.abort();
        self.queue.close();
    }
}

#[cfg(test)]
mod tests;
