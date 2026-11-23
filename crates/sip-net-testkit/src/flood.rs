//! **TEST-ONLY.** [`Flood`]: an endpoint whose inbound queue an overload holds
//! non-empty, as an intake shed holds a real queue at its watermark. The flood
//! is `total` copies of one datagram, queued behind the endpoint's own
//! traffic; each copy the reader takes is replaced by the next until `total`
//! were taken, so the reader finds a datagram ready on every read while the
//! flood lasts. What the reader sends is logged with the copies it had taken
//! by then.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use sip_message::Bytes;

use crate::net::UdpEndpoint;
use crate::queue::PacketQueue;
use crate::types::{SendError, UdpEndpointCounters, UdpPacket};

/// The handle of a flood over an endpoint ([`Flood::over`]). Clone-cheap.
#[derive(Clone)]
pub struct Flood {
    shared: Arc<Shared>,
}

struct Shared {
    /// The copies queued and not yet taken: at most the depth `start` names.
    queue: PacketQueue,
    datagram: Bytes,
    src: SocketAddr,
    total: u64,
    offered: AtomicU64,
    taken: AtomicU64,
    /// Every datagram the endpoint sent, with the copies taken before it.
    sent: Mutex<Vec<(u64, Bytes)>>,
}

impl Shared {
    fn offer_next(&self) {
        if self.offered.fetch_add(1, Ordering::SeqCst) < self.total {
            let copy = UdpPacket {
                raw: self.datagram.clone(),
                src: self.src,
                arrival_ms: 0,
                parsed: None,
            };
            self.queue.offer(copy);
        }
    }

    fn took(&self, packet: Option<UdpPacket>) -> Option<UdpPacket> {
        if packet.is_some() {
            self.taken.fetch_add(1, Ordering::SeqCst);
            self.offer_next();
        }
        packet
    }
}

impl Flood {
    /// `inner`, read ahead of a flood of `total` copies of `datagram` from
    /// `src` that waits for [`start`](Self::start), and the flood's handle.
    pub fn over(
        inner: Box<dyn UdpEndpoint>,
        datagram: &[u8],
        src: SocketAddr,
        total: u64,
    ) -> (Box<dyn UdpEndpoint>, Flood) {
        let shared = Arc::new(Shared {
            queue: PacketQueue::new(usize::MAX),
            datagram: Bytes::copy_from_slice(datagram),
            src,
            total,
            offered: AtomicU64::new(0),
            taken: AtomicU64::new(0),
            sent: Mutex::new(Vec::new()),
        });
        let flood = Flood { shared };
        (Box::new(FloodedEndpoint { inner, flood: flood.clone() }), flood)
    }

    /// Queue `depth` copies, the depth the flood then holds while it lasts.
    pub fn start(&self, depth: usize) {
        for _ in 0..depth {
            self.shared.offer_next();
        }
    }

    /// The copies the reader took.
    pub fn taken(&self) -> u64 {
        self.shared.taken.load(Ordering::SeqCst)
    }

    /// Whether a copy is still to be taken.
    pub fn lasts(&self) -> bool {
        self.taken() < self.shared.total
    }

    /// Every datagram the endpoint sent, oldest first, each with the copies
    /// the reader had taken before it left.
    pub fn sent(&self) -> Vec<(u64, Bytes)> {
        self.shared.sent.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }
}

struct FloodedEndpoint {
    inner: Box<dyn UdpEndpoint>,
    flood: Flood,
}

#[async_trait]
impl UdpEndpoint for FloodedEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        let entry = (self.flood.taken(), Bytes::copy_from_slice(buf));
        self.flood.shared.sent.lock().unwrap_or_else(PoisonError::into_inner).push(entry);
        self.inner.send_to(buf, dst).await
    }

    async fn recv(&self) -> Option<UdpPacket> {
        let shared = &self.flood.shared;
        tokio::select! {
            biased;
            own = self.inner.recv() => own,
            copy = shared.queue.take() => shared.took(copy),
        }
    }

    fn try_recv(&self) -> Option<UdpPacket> {
        let shared = &self.flood.shared;
        self.inner.try_recv().or_else(|| shared.took(shared.queue.poll()))
    }

    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    fn queue_depth(&self) -> usize {
        self.inner.queue_depth() + self.flood.shared.queue.depth()
    }

    fn queue_wait(&self) -> Duration {
        self.inner.queue_wait()
    }

    fn queue_max(&self) -> usize {
        self.inner.queue_max()
    }

    fn counters(&self) -> UdpEndpointCounters {
        self.inner.counters()
    }
}
