//! Real `dgram`-backed `SignalingNetwork` (port of `SignalingNetwork.real.ts`),
//! over `tokio::net::UdpSocket`.
//!
//! Each `bind_udp` opens a real UDP socket and spawns a receive task that
//! pumps `recv_from` into the endpoint's bounded [`PacketQueue`], applying the
//! pre-ingress hook at arrival time exactly as the source's `socket.on(
//! "message")` handler did. Trace recording is NOT here — in this port the
//! typed `Recorder` channel (the recording decorator in `contracts.rs`) is the
//! single recording path, replacing the source's `realTracing` boolean split.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use layer_harness::time::now_ms;
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;

use crate::net::{Counters, SignalingNetwork, UdpEndpoint};
use crate::queue::PacketQueue;
use crate::types::{
    BindError, BindErrorReason, BindUdpOpts, PreIngressAction, PreIngressHook, SendError,
    UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};

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
            reason: BindErrorReason::OsError,
            addr: opts.addr,
            message: e.to_string(),
        };
        // `reuse_port` (SO_REUSEPORT) needs a socket2 detour — tokio's
        // `UdpSocket::bind` has no knob for it. N reuse-port sockets on one
        // port shard the recv path across N tasks; the kernel flow-hashes on
        // the 4-tuple, so all datagrams from one src:port land on ONE socket
        // and per-flow ordering (INVITE→CANCEL, retransmits) is preserved.
        let socket = if opts.reuse_port {
            let domain = socket2::Domain::for_address(opts.addr);
            let raw = socket2::Socket::new(domain, socket2::Type::DGRAM, Some(socket2::Protocol::UDP))
                .map_err(os_err)?;
            raw.set_reuse_port(true).map_err(os_err)?;
            // tokio's reactor requires the fd non-blocking.
            raw.set_nonblocking(true).map_err(os_err)?;
            raw.bind(&opts.addr.into()).map_err(os_err)?;
            UdpSocket::from_std(raw.into()).map_err(os_err)?
        } else {
            UdpSocket::bind(opts.addr).await.map_err(os_err)?
        };
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

/// The receive pump. Mirrors the source's `socket.on("message", ...)` handler:
/// depth-aware pre-ingress dispatch, tail-drop on a full queue.
async fn recv_loop(
    socket: Arc<UdpSocket>,
    queue: Arc<PacketQueue>,
    counters: Arc<Counters>,
    pre_ingress: Option<PreIngressHook>,
) {
    let mut buf = vec![0u8; 65_536];
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
                let _ = socket.send_to(&bytes, src).await;
            }
            PreIngressAction::Accept => {
                let pkt = UdpPacket {
                    raw,
                    src,
                    arrival_ms: now_ms(),
                };
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
        self.socket
            .send_to(buf, dst)
            .await
            .map(|_| ())
            .map_err(|e| SendError {
                message: e.to_string(),
            })
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
