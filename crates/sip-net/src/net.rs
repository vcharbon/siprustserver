//! The `SignalingNetwork` + `UdpEndpoint` trait pair — the DI seam for the
//! network layer (port of `SignalingNetworkApi` + `UdpEndpoint`).
//!
//! The trait is the seam consumers depend on; implementations (real,
//! simulated) and decorators (recording, paranoid) all satisfy it. Methods
//! are reshaped to tokio idioms: `bind_udp` returns a boxed `UdpEndpoint`
//! trait object (so decorators can wrap it), `send_to` mirrors
//! `UdpSocket::send_to`, and the `Stream` of the source is replaced by
//! receiver-style `recv` / `try_recv`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use async_trait::async_trait;

use crate::types::{BindError, BindUdpOpts, SendError, UdpEndpointCounters, UdpPacket, UndeliveredPacket};

/// A bound UDP endpoint. Owns its inbound queue (the `recv` side) and the
/// means to send (`send_to`). Dropping it releases the underlying socket /
/// routing-table slot.
#[async_trait]
pub trait UdpEndpoint: Send + Sync {
    /// Fire-and-forget send (mirrors `tokio::net::UdpSocket::send_to`).
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError>;

    /// Await the next inbound packet (the TS `take`). `None` once the
    /// endpoint's queue is closed and drained.
    async fn recv(&self) -> Option<UdpPacket>;

    /// Non-blocking inbound poll (the TS `poll`).
    fn try_recv(&self) -> Option<UdpPacket>;

    /// The bound local address (single source of truth for Via/Contact
    /// stamping in later layers).
    fn local_addr(&self) -> SocketAddr;

    /// Current inbound-queue depth.
    fn queue_depth(&self) -> usize;

    /// The bind's configured `queue_max`.
    fn queue_max(&self) -> usize;

    /// Snapshot of the live per-endpoint counters.
    fn counters(&self) -> UdpEndpointCounters;

    /// Install a delivery-time recording tap on this endpoint's inbox
    /// (newkahneed-036 ask A). Returns `false` when the impl has no tappable
    /// inbox — the recording decorator then falls back to recv-time recording.
    /// Concrete endpoints (real / simulated / loadgen mux) delegate to their
    /// `PacketQueue`; pure test doubles keep the default.
    fn install_recv_tap(&self, tap: crate::types::RecvTap) -> bool {
        let _ = tap;
        false
    }
}

/// Forwarding impl so a single bound endpoint can be **shared**: one owner is
/// boxed and moved into the consumer that drives the recv loop (e.g.
/// `B2buaCore::spawn`, which takes `Box<dyn UdpEndpoint>`), while clones of the
/// `Arc` feed read-only live getters elsewhere — notably the
/// `UdpTransportMetrics` `queueDepth`/`dropsTailDrop` proxies, which (per the TS
/// `UdpTransport` facade) close over the very endpoint the transport sends
/// through. Every method delegates to the inner; sharing the recv side is safe
/// because the underlying endpoints take `&self` and serialise internally
/// (the simulated `PacketQueue` and the real socket are both `Send + Sync`).
#[async_trait]
impl UdpEndpoint for std::sync::Arc<dyn UdpEndpoint> {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        (**self).send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        (**self).recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        (**self).try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
        (**self).local_addr()
    }
    fn queue_depth(&self) -> usize {
        (**self).queue_depth()
    }
    fn queue_max(&self) -> usize {
        (**self).queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        (**self).counters()
    }
    fn install_recv_tap(&self, tap: crate::types::RecvTap) -> bool {
        (**self).install_recv_tap(tap)
    }
}

/// Abstraction over the SIP-signaling network (port of `SignalingNetworkApi`).
#[async_trait]
pub trait SignalingNetwork: Send + Sync {
    /// Bind a UDP endpoint. The returned endpoint lives until dropped.
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError>;

    /// Drain packets that could not be delivered (simulated fabric only;
    /// real returns empty). Used by the layer-close audit.
    async fn drain_undeliverable(&self) -> Vec<UndeliveredPacket>;

    /// `Some(ms)` on the simulated fabric (its per-hop transit delay), `None`
    /// on real. The audit uses `None` to skip in-memory structural checks.
    fn transit_delay_ms(&self) -> Option<u64>;

    /// In-flight transit count (simulated only; real is always 0).
    fn in_flight(&self) -> i64;

    /// Adjust the in-flight counter (used by a buffered-send wrapper to keep
    /// the simulated quiescence wait accurate). No-op on real.
    fn bump_in_flight(&self, delta: i64);

    /// Live `(addr, depth)` for every bound endpoint (simulated only; real
    /// returns empty). Drives the layer-close queue-leak check.
    fn queue_depths(&self) -> Vec<(SocketAddr, usize)>;

    /// Bounded wait until `in_flight` reaches 0 or `timeout` elapses. No-op on
    /// real (no in-memory transit to drain).
    async fn await_in_flight(&self, timeout: Duration);
}

/// Shared per-endpoint counters (the four `UdpEndpointCounters` fields as live
/// atomics). Both the real recv task and the simulated `deliver` path bump
/// these; `snapshot` produces the public [`UdpEndpointCounters`].
#[derive(Debug, Default)]
pub struct Counters {
    pub enqueued: AtomicU64,
    pub tail_dropped: AtomicU64,
    pub pre_ingress_dropped: AtomicU64,
    pub pre_ingress_replies: AtomicU64,
}

impl Counters {
    pub fn snapshot(&self) -> UdpEndpointCounters {
        UdpEndpointCounters {
            enqueued: self.enqueued.load(Ordering::Relaxed),
            tail_dropped: self.tail_dropped.load(Ordering::Relaxed),
            pre_ingress_dropped: self.pre_ingress_dropped.load(Ordering::Relaxed),
            pre_ingress_replies: self.pre_ingress_replies.load(Ordering::Relaxed),
        }
    }
}
