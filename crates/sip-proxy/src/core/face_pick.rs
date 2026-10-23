//! [`FacePickSendEndpoint`] — the dual-face egress picker as a send seam.

use std::net::SocketAddr;
use std::sync::Arc;

use sip_net::UdpEndpoint;

use crate::face::FaceCidrs;

/// A send-only `UdpEndpoint` that picks the internal/external socket per
/// destination IP — the face picker as a seam, for consumers that hold "an
/// endpoint" and send to already-resolved addresses (the [`NamedForwarder`](crate::resolver::NamedForwarder)).
/// Never used for recv (the core recv-selects the real sockets).
pub(super) struct FacePickSendEndpoint {
    pub(super) internal: Arc<dyn UdpEndpoint>,
    pub(super) external: Arc<dyn UdpEndpoint>,
    pub(super) int_cidrs: FaceCidrs,
}

#[async_trait::async_trait]
impl UdpEndpoint for FacePickSendEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), sip_net::SendError> {
        if self.int_cidrs.contains_ip(dst.ip()) {
            self.internal.send_to(buf, dst).await
        } else {
            self.external.send_to(buf, dst).await
        }
    }
    async fn recv(&self) -> Option<sip_net::UdpPacket> {
        std::future::pending().await
    }
    fn try_recv(&self) -> Option<sip_net::UdpPacket> {
        None
    }
    fn local_addr(&self) -> SocketAddr {
        self.internal.local_addr()
    }
    fn egress_local_addr(&self, dst: SocketAddr) -> SocketAddr {
        if self.int_cidrs.contains_ip(dst.ip()) {
            self.internal.local_addr()
        } else {
            self.external.local_addr()
        }
    }
    fn queue_depth(&self) -> usize {
        0
    }
    fn queue_max(&self) -> usize {
        0
    }
    fn counters(&self) -> sip_net::UdpEndpointCounters {
        sip_net::UdpEndpointCounters::default()
    }
}
