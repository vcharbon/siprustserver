//! A transaction layer that takes its datagrams slowly: [`PacedInbound`] wraps
//! the SUT's bound endpoint and spaces its `recv` by the [`InboundPace`] the
//! test holds, so what arrives meanwhile waits in the inbound UDP queue, as it
//! does in front of a process short of CPU. Sends, and the queue's own
//! readings, pass through.

use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use sip_net::{SendError, UdpEndpoint, UdpEndpointCounters, UdpPacket};
use tokio::time::Instant;

#[derive(Default)]
struct Pacing {
    /// The time between two takes; zero runs free.
    per_take: Duration,
    /// The earliest instant of the next take.
    next_take: Option<Instant>,
}

/// The pace of a SUT's inbound takes, shared by the test and the endpoint.
/// Free until set.
#[derive(Clone, Default)]
pub struct InboundPace(Arc<Mutex<Pacing>>);

impl InboundPace {
    pub fn new() -> Self {
        Self::default()
    }

    /// Space the takes `per_take` apart from now on; zero runs free again.
    pub fn set(&self, per_take: Duration) {
        let mut pacing = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        pacing.per_take = per_take;
        pacing.next_take = None;
    }

    fn next_take(&self) -> Option<Instant> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).next_take
    }

    fn taken(&self) {
        let mut pacing = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        pacing.next_take = (!pacing.per_take.is_zero()).then(|| Instant::now() + pacing.per_take);
    }
}

/// An endpoint whose `recv` hands over one datagram per interval of its
/// [`InboundPace`]. Cancelling a `recv` loses no datagram: the wait comes
/// before the take.
pub struct PacedInbound {
    inner: Box<dyn UdpEndpoint>,
    pace: InboundPace,
}

impl PacedInbound {
    pub fn new(inner: Box<dyn UdpEndpoint>, pace: InboundPace) -> Self {
        Self { inner, pace }
    }
}

#[async_trait]
impl UdpEndpoint for PacedInbound {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.inner.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        if let Some(at) = self.pace.next_take() {
            tokio::time::sleep_until(at).await;
        }
        let packet = self.inner.recv().await;
        self.pace.taken();
        packet
    }
    fn peek_queued(&self) -> Vec<UdpPacket> {
        self.inner.peek_queued()
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.inner.try_recv()
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
    fn queue_depth(&self) -> usize {
        self.inner.queue_depth()
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
    fn install_recv_tap(&self, tap: sip_net::RecvTap) -> bool {
        self.inner.install_recv_tap(tap)
    }
    fn install_send_tap(&self, tap: sip_net::SendTap) -> bool {
        self.inner.install_send_tap(tap)
    }
}
