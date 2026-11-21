//! The proxy's periodic tasks beside its recv shards: the load observer's
//! stale-payload sweep and the self-gate's load sampler. Each owns no
//! per-call state and rebuilds nothing on a new run, so a run that panics is
//! restarted and counted ([`sip_proxy::supervise`]).

use std::sync::Arc;
use std::time::Duration;

use sip_clock::Clock;
use sip_proxy::load_observer::{sweep_alive, WorkerLoadObserver};
use sip_proxy::observability::ProxyMetrics;
use sip_proxy::registry::WorkerRegistry;
use sip_proxy::self_gate::EluCpsGate;
use sip_proxy::supervise::{self, SELF_GATE_SAMPLER, STALE_FLOOR_SWEEP};
use tokio::task::JoinHandle;

const STALE_SWEEP_PERIOD: Duration = Duration::from_secs(1);

/// Spawns the supervised stale-payload sweep: every `STALE_SWEEP_PERIOD`,
/// `sweep_alive` decreases the AIMD cap of each Alive worker whose last
/// `X-Overload` payload is older than `payload_stale_ms`, and adds the floored
/// count to the `stale_decrease` counter. A worker that answers its probes is
/// never floored: every `200` carries a payload, a refusing worker's included,
/// and the probe cycle sits inside the stale window. See ADR-0050 §6.
pub fn spawn_stale_floor_sweep(
    observer: Arc<WorkerLoadObserver>,
    registry: Arc<dyn WorkerRegistry>,
    clock: Clock,
    metrics: Arc<ProxyMetrics>,
) -> JoinHandle<()> {
    supervise::spawn(STALE_FLOOR_SWEEP, move || {
        let (observer, registry) = (observer.clone(), registry.clone());
        let (clock, metrics) = (clock.clone(), metrics.clone());
        async move {
            #[expect(clippy::disallowed_methods, reason = "STALE_SWEEP_PERIOD is not zero")]
            let mut t = tokio::time::interval(STALE_SWEEP_PERIOD);
            t.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            t.tick().await;
            loop {
                t.tick().await;
                let floored = sweep_alive(&observer, registry.as_ref(), clock.now_ms());
                if floored > 0 {
                    metrics.record_overload_stale_decrease(floored);
                }
            }
        }
    })
}

/// Spawns the supervised self-gate sampler: each tick of the gate's sampler
/// interval drains the shared intake-age recorder's window max into the
/// gate's EWMA.
pub fn spawn_self_gate_sampler(gate: EluCpsGate) -> JoinHandle<()> {
    supervise::spawn(SELF_GATE_SAMPLER, move || {
        let gate = gate.clone();
        async move {
            let period = gate.sampler_interval().max(Duration::from_millis(1));
            #[expect(clippy::disallowed_methods, reason = "the period is held above zero")]
            let mut tick = tokio::time::interval(period);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await;
            loop {
                tick.tick().await;
                gate.sample();
            }
        }
    })
}

#[cfg(test)]
mod tests;
