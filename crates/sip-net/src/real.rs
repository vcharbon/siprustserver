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
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::fragmentation::pin_fragmentation;
use crate::net::{Counters, SignalingNetwork, UdpEndpoint};
use crate::queue::PacketQueue;
use crate::types::{
    BindError, BindErrorReason, BindUdpOpts, PreIngressAction, PreIngressHook, SendError,
    UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};

/// Build the UDP socket every real bind stands on. socket2, always, because
/// two things this stack needs have no tokio knob: the path-MTU-discovery mode
/// (ADR-0027 — signalling is UDP-only, so an oversize datagram MUST fragment at
/// IP rather than fail the send) and `SO_REUSEPORT`. N reuse-port sockets on
/// one port shard the recv path across N tasks; the kernel flow-hashes on the
/// 4-tuple, so all datagrams from one src:port land on ONE socket and per-flow
/// ordering (INVITE→CANCEL, retransmits) is preserved. `send_buffer` is the
/// requested `SO_SNDBUF` (`None` keeps the kernel default). Public so a test
/// reads the options back off the very socket the bind path produces.
pub fn build_bound_socket(
    addr: SocketAddr,
    reuse_port: bool,
    send_buffer: Option<usize>,
) -> std::io::Result<socket2::Socket> {
    let domain = socket2::Domain::for_address(addr);
    let raw = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))?;
    if reuse_port {
        raw.set_reuse_port(true)?;
    }
    if let Some(bytes) = send_buffer {
        raw.set_send_buffer_size(bytes)?;
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
        let raw = build_bound_socket(opts.addr, opts.reuse_port, opts.send_buffer_bytes)
            .map_err(os_err)?;
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
const _: () = assert!(RECV_BUF_LEN > crate::types::MAX_UDP_PAYLOAD);

/// The receive pump. Mirrors the source's `socket.on("message", ...)` handler:
/// depth-aware pre-ingress dispatch, tail-drop on a full queue.
async fn recv_loop(
    socket: Arc<UdpSocket>,
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    pre_ingress: Option<PreIngressHook>,
    clock: Clock,
) {
    let mut buf = vec![0u8; RECV_BUF_LEN];
    // A `recv_from` error is treated as terminal (socket closed) and ends the
    // pump. The source logged and continued on transient errors; for our
    // test/loopback usage surfacing an error here is effectively terminal.
    while let Ok((n, src)) = socket.recv_from(&mut buf).await {
        let raw = buf[..n].to_vec();
        let depth = queue.depth();
        let action = match &pre_ingress {
            Some(hook) => hook(&raw, src, depth),
            None => PreIngressAction::Accept,
        };
        match action {
            PreIngressAction::Drop => {
                counters.pre_ingress_dropped.fetch_add(1, Ordering::Relaxed);
            }
            PreIngressAction::Reply(bytes) => {
                counters.pre_ingress_replies.fetch_add(1, Ordering::Relaxed);
                // The reply is best-effort — the pump must keep receiving — and
                // a failure is COUNTED, never printed: this crate reports
                // through counters alone, and a peer that rejects every reply
                // would otherwise print once per datagram on the receive path.
                if send_now(&socket, &counters, &bytes, src).is_err() {
                    counters.pre_ingress_reply_failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            PreIngressAction::Accept => {
                let pkt = UdpPacket { raw, src, arrival_ms: clock.now_ms().max(0) as u64 };
                if queue.offer(pkt) {
                    counters.enqueued.fetch_add(1, Ordering::Relaxed);
                } else {
                    counters.tail_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
        }
    }
    // The pump is terminating (recv_from error). CLOSE the queue so the
    // endpoint's `recv()` resolves to None and the owner task can wind down.
    // Without this the layer goes permanently DEAF — sends still work, nothing is
    // ever received, no panic/metric/readiness change — because the endpoint is
    // owned by the very owner task blocked on `recv()`, so its `Drop` (the only
    // other `close` site) can never run until that `recv()` returns.
    queue.close();
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
        self.counters.snapshot()
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
