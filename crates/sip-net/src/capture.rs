//! [`CaptureEndpoint`]: a decorator appending each datagram its endpoint sends
//! and delivers to a pcapng file ([`crate::pcapng`]), the packet capture a
//! process makes of its own signalling with no privilege and no capture tool.
//!
//! Each capture opened on a file appends a section of its own, so a file that
//! several runs of a process captured to stays one readable capture. A
//! datagram is recorded when it is delivered to the consumer (`recv`,
//! `try_recv`) or handed to the transport (`send_to`), stamped with the wall
//! clock then: what a pre-ingress hook answers or drops on the receive path
//! never reaches the consumer and is not recorded.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use crate::net::UdpEndpoint;
use crate::pcapng;
use crate::types::{RecvTap, SendError, SendTap, SocketBuffers, UdpEndpointCounters, UdpPacket};

/// An endpoint whose datagrams are appended to a capture file, every other
/// call forwarded to the endpoint it wraps.
pub struct CaptureEndpoint {
    inner: Arc<dyn UdpEndpoint>,
    file: File,
    /// Set once a write failed: the capture stops, said once in the log.
    failed: AtomicBool,
}

impl CaptureEndpoint {
    /// `inner` captured to the file at `path`, created where absent and
    /// appended to, its section opened. `Err` where the file cannot be opened
    /// or written.
    pub fn open(inner: Arc<dyn UdpEndpoint>, path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        (&file).write_all(&pcapng::section_header())?;
        Ok(Self { inner, file, failed: AtomicBool::new(false) })
    }

    /// Appends `payload`, from `src` to `dst`, as one block: one write on a
    /// file opened to append, so blocks of concurrent senders never interleave.
    fn record(&self, src: SocketAddr, dst: SocketAddr, payload: &[u8]) {
        if self.failed.load(Ordering::Relaxed) {
            return;
        }
        let Some(block) = pcapng::packet_block(now_us(), src, dst, payload) else { return };
        if let Err(error) = (&self.file).write_all(&block) {
            self.failed.store(true, Ordering::Relaxed);
            tracing::warn!(%error, "the packet capture stopped: its file refused a write");
        }
    }

    /// Records `packet`, delivered to this endpoint's own address.
    fn delivered(&self, packet: Option<UdpPacket>) -> Option<UdpPacket> {
        if let Some(packet) = &packet {
            self.record(packet.src, self.inner.local_addr(), &packet.raw);
        }
        packet
    }
}

/// Microseconds since the UNIX epoch on the wall clock; 0 before it.
fn now_us() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| u64::try_from(since.as_micros()).unwrap_or(u64::MAX))
}

#[async_trait]
impl UdpEndpoint for CaptureEndpoint {
    async fn send_to(&self, buf: &[u8], dst: SocketAddr) -> Result<(), SendError> {
        self.record(self.inner.egress_local_addr(dst), dst, buf);
        self.inner.send_to(buf, dst).await
    }
    async fn recv(&self) -> Option<UdpPacket> {
        let packet = self.inner.recv().await;
        self.delivered(packet)
    }
    fn peek_queued(&self) -> Vec<UdpPacket> {
        self.inner.peek_queued()
    }
    fn try_recv(&self) -> Option<UdpPacket> {
        self.delivered(self.inner.try_recv())
    }
    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }
    fn egress_local_addr(&self, dst: SocketAddr) -> SocketAddr {
        self.inner.egress_local_addr(dst)
    }
    fn queue_depth(&self) -> usize {
        self.inner.queue_depth()
    }
    fn queue_max(&self) -> usize {
        self.inner.queue_max()
    }
    fn counters(&self) -> UdpEndpointCounters {
        self.inner.counters()
    }
    fn socket_buffers(&self) -> Option<SocketBuffers> {
        self.inner.socket_buffers()
    }
    fn install_recv_tap(&self, tap: RecvTap) -> bool {
        self.inner.install_recv_tap(tap)
    }
    fn install_send_tap(&self, tap: SendTap) -> bool {
        self.inner.install_send_tap(tap)
    }
}
