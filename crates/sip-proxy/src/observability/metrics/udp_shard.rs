//! One recv shard's endpoint state, read off its sockets and summed over its
//! faces.

use sip_net::UdpEndpoint;

/// One recv shard's endpoint state: receive-queue gauges and the endpoints'
/// lifetime counters, every field summed over the shard's faces.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UdpShardStats {
    pub queue_depth: u64,
    pub queue_max: u64,
    pub enqueued: u64,
    pub tail_dropped: u64,
    pub intake_shed: u64,
    pub send_would_block: u64,
    /// Datagrams the kernel dropped on the shard's sockets before the recv
    /// pump read them (a full `SO_RCVBUF`).
    pub kernel_rx_dropped: u64,
    /// Datagrams whose pre-ingress hook (the intake shed) panicked: each was
    /// accepted, each is a hook defect.
    pub pre_ingress_panics: u64,
}

impl UdpShardStats {
    /// The shard's stats over its internal face and, in dual-face mode, its
    /// external face, where callers arrive.
    pub fn of_faces(internal: &dyn UdpEndpoint, external: Option<&dyn UdpEndpoint>) -> Self {
        let mut stats = Self::of_endpoint(internal);
        if let Some(ext) = external {
            stats += Self::of_endpoint(ext);
        }
        stats
    }

    fn of_endpoint(ep: &dyn UdpEndpoint) -> Self {
        let c = ep.counters();
        Self {
            queue_depth: ep.queue_depth() as u64,
            queue_max: ep.queue_max() as u64,
            enqueued: c.enqueued,
            tail_dropped: c.tail_dropped,
            intake_shed: c.pre_ingress_dropped,
            send_would_block: c.send_would_block,
            kernel_rx_dropped: c.kernel_rx_dropped,
            pre_ingress_panics: c.pre_ingress_panics,
        }
    }
}

impl super::ProxyMetrics {
    /// Publish one recv shard's endpoint receive-queue state (gauges) and
    /// lifetime counters (monotonic, endpoint-owned — stored, not accumulated).
    /// Single-socket deployments are just `shard = 0`.
    pub fn set_udp_endpoint_stats(&self, shard: usize, stats: UdpShardStats) {
        crate::poison::locked(&self.udp_shards, crate::poison::UDP_SHARD_STATS)
            .insert(shard, stats);
    }

    /// Cross-shard aggregate.
    pub(super) fn udp_totals(&self) -> UdpShardStats {
        let mut t = UdpShardStats::default();
        for v in crate::poison::locked(&self.udp_shards, crate::poison::UDP_SHARD_STATS).values() {
            t += *v;
        }
        t
    }
}

impl std::ops::AddAssign for UdpShardStats {
    fn add_assign(&mut self, o: Self) {
        self.queue_depth += o.queue_depth;
        self.queue_max += o.queue_max;
        self.enqueued += o.enqueued;
        self.tail_dropped += o.tail_dropped;
        self.intake_shed += o.intake_shed;
        self.send_would_block += o.send_would_block;
        self.kernel_rx_dropped += o.kernel_rx_dropped;
        self.pre_ingress_panics += o.pre_ingress_panics;
    }
}
