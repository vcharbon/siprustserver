//! The health sampler: every 2 s it publishes the recv shards' stall gauge,
//! the worker-health and acceptance gauges and the discovery's own health, so
//! an empty or unprobed pool and the reason for it are observable in
//! Prometheus, not only through readiness. A shard's stall and its recovery
//! are each logged once.

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_proxy::liveness::ShardPulse;
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::{WorkerHealth, WorkerRegistry};
use topology::K8sMembership;

/// What the sampler reads and where it publishes.
pub struct HealthSampler {
    pub registry: Arc<dyn WorkerRegistry>,
    /// The EndpointSlice watch, `None` for a fixed pool.
    pub discovery: Option<Arc<K8sMembership>>,
    pub metrics: Arc<ProxyMetrics>,
    pub pulse: Arc<ShardPulse>,
    pub clock: Clock,
    pub stall_ms: u64,
}

/// How often the sampler publishes.
const PERIOD: Duration = Duration::from_secs(2);

impl HealthSampler {
    /// Spawn the sampling loop; it runs for the life of the process, a run
    /// that panics restarted and counted ([`sip_proxy::supervise`]).
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let sampler = Arc::new(self);
        sip_proxy::supervise::spawn(sip_proxy::supervise::HEALTH_SAMPLER, move || {
            let sampler = sampler.clone();
            async move {
                #[expect(clippy::disallowed_methods, reason = "PERIOD is not zero")]
                let mut ticker = tokio::time::interval(PERIOD);
                let mut stalled_before: Vec<usize> = Vec::new();
                loop {
                    ticker.tick().await;
                    stalled_before = sampler.sample(stalled_before);
                }
            }
        })
    }

    /// One sample; `stalled_before` is the previous sample's stalled shards,
    /// and the current ones are returned.
    fn sample(&self, stalled_before: Vec<usize>) -> Vec<usize> {
        let m = &self.metrics;
        let now_ms = u64::try_from(self.clock.now_ms()).unwrap_or(0);
        let stalled = self.pulse.stalled(now_ms, self.stall_ms);
        m.set_recv_shards_stalled(stalled.len() as u64);
        if stalled != stalled_before {
            if stalled.is_empty() {
                tracing::info!(shards = ?stalled_before, "recv shards turning again");
            } else {
                tracing::error!(
                    shards = ?stalled,
                    threshold_ms = self.stall_ms,
                    "recv shard(s) stalled on one packet — /healthz, /readyz and /vrrp fail"
                );
            }
        }
        let (mut alive, mut unknown, mut dead, mut accepting) = (0, 0, 0, 0);
        for w in self.registry.snapshot() {
            match w.health {
                WorkerHealth::Alive => alive += 1,
                WorkerHealth::Unknown => unknown += 1,
                WorkerHealth::Dead => dead += 1,
            }
            accepting += u64::from(w.accepts_new_calls());
        }
        m.set_worker_health_counts(alive, unknown, dead);
        m.set_worker_acceptance_counts(accepting, alive - accepting);
        match self.discovery.as_ref().map(|watch| watch.watch_health()) {
            // An informer whose task ended watches nothing, as one with no client.
            Some(h) => m.set_discovery(h.client_failing || h.ended, h.broken_since_unix_ms),
            None => m.set_discovery(false, None),
        }
        stalled
    }
}
