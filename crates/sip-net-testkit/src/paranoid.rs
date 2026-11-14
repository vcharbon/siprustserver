//! **TEST-ONLY.** [`ParanoidSignalingNetwork`]: the paranoid-inputs decorator
//! of `SignalingNetwork` (ADR-0004), the outer one of the canonical
//! composition ([`crate::contracts::with_all_contracts`]).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use layer_harness::ParanoidViolation;
use sip_net::max_udp_payload;

use crate::net::{SignalingNetwork, UdpEndpoint};
use crate::types::{
    BindError, BindUdpOpts, SendError, UdpEndpointCounters, UdpPacket, UndeliveredPacket,
};

/// Caller-side precondition checks. A violation is a programmer error and is
/// raised by panic. Checks (all always-on,
/// µs-scale):
///
///   - `PA2_bindOpts_queueMax`  bind_udp queue_max ≥ 1
///   - `PA3_send_validDest`     send dst port ≠ 0
///   - `PA4_send_msgBuffer`     send buf non-empty
///   - `PA5_send_msgSizeBound`  send buf.len() ≤ the largest UDP payload of dst's family
///
/// (PA1 — a valid bind address — is enforced by the `SocketAddr` type itself:
/// an ip and a u16 port always exist, and port 0 = ephemeral is allowed.)
#[derive(Clone)]
pub struct ParanoidSignalingNetwork {
    inner: Arc<dyn SignalingNetwork>,
}

impl ParanoidSignalingNetwork {
    pub fn new(inner: Arc<dyn SignalingNetwork>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl SignalingNetwork for ParanoidSignalingNetwork {
    async fn bind_udp(&self, opts: BindUdpOpts) -> Result<Box<dyn UdpEndpoint>, BindError> {
        if opts.queue_max == 0 {
            panic!(
                "{}",
                ParanoidViolation::new(
                    "PA2_bindOpts_queueMax",
                    "bind_udp queue_max must be a positive integer (got 0)",
                )
            );
        }
        let endpoint = self.inner.bind_udp(opts).await?;
        Ok(Box::new(ParanoidEndpoint { inner: endpoint }))
    }

    async fn drain_undeliverable(&self) -> Vec<UndeliveredPacket> {
        self.inner.drain_undeliverable().await
    }
    fn transit_delay_ms(&self) -> Option<u64> {
        self.inner.transit_delay_ms()
    }
    fn in_flight(&self) -> i64 {
        self.inner.in_flight()
    }
    fn bump_in_flight(&self, delta: i64) {
        self.inner.bump_in_flight(delta)
    }
    fn queue_depths(&self) -> Vec<(SocketAddr, usize)> {
        self.inner.queue_depths()
    }
    async fn await_in_flight(&self, timeout: Duration) {
        self.inner.await_in_flight(timeout).await
    }
}

struct ParanoidEndpoint {
    inner: Box<dyn UdpEndpoint>,
}

#[async_trait]
impl UdpEndpoint for ParanoidEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        if dst.port() == 0 {
            panic!(
                "{}",
                ParanoidViolation::new(
                    "PA3_send_validDest",
                    format!("send dst port must be in 1..=65535 (got {})", dst.port()),
                )
            );
        }
        if buf.is_empty() {
            panic!(
                "{}",
                ParanoidViolation::new("PA4_send_msgBuffer", "send buf must be non-empty")
            );
        }
        let max = max_udp_payload(dst);
        if buf.len() > max {
            panic!(
                "{}",
                ParanoidViolation::new(
                    "PA5_send_msgSizeBound",
                    format!("send buf.len()={} exceeds the largest UDP payload {max}", buf.len()),
                )
            );
        }
        self.inner.send_to(buf, dst).await
    }

    async fn recv(&self) -> Option<UdpPacket> {
        self.inner.recv().await
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.inner.try_recv()
    }
    fn peek_queued(&self) -> Vec<UdpPacket> {
        self.inner.peek_queued()
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
    fn socket_buffers(&self) -> Option<crate::types::SocketBuffers> {
        self.inner.socket_buffers()
    }
}
