//! The proxy's probe answers (ADR-0012 D4, ADR-0046 D4): `/readyz` is fit for
//! traffic only with at least one `Alive` worker, every recv shard turning,
//! and no drain latched; `/healthz` is live while every recv shard turns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use probe_http::{LiveFn, ProbeState, ReadyFn};
use sip_clock::Clock;
use sip_proxy::liveness::ShardPulse;
use sip_proxy::registry::{WorkerHealth, WorkerRegistry};

/// Draining once SIGTERM latched; else NotReady while a recv shard is
/// stalled or no worker is `Alive`; else Ready.
pub fn probe_state(
    draining: bool,
    shard_stalled: bool,
    registry: &dyn WorkerRegistry,
) -> ProbeState {
    if draining {
        ProbeState::Draining
    } else if shard_stalled {
        ProbeState::NotReady
    } else if registry.snapshot().iter().any(|w| w.health == WorkerHealth::Alive) {
        ProbeState::Ready
    } else {
        ProbeState::NotReady
    }
}

/// The readiness and liveness the probe server answers with, read from
/// `pulse` on `clock`: a shard holding one packet longer than `stall_ms`
/// fails both, so the VIP's health check moves the address and the
/// livenessProbe restarts the process. A shard waiting on an empty socket is
/// never stalled.
pub fn probes(
    draining: Arc<AtomicBool>,
    pulse: Arc<ShardPulse>,
    clock: Clock,
    stall_ms: u64,
    registry: Arc<dyn WorkerRegistry>,
) -> (ReadyFn, LiveFn) {
    let stalled = {
        let pulse = pulse.clone();
        let clock = clock.clone();
        move || !pulse.stalled(clock.now_ms().max(0) as u64, stall_ms).is_empty()
    };
    let ready_stalled = stalled.clone();
    let ready: ReadyFn = Arc::new(move || {
        probe_state(draining.load(Ordering::Relaxed), ready_stalled(), registry.as_ref())
    });
    let live: LiveFn = Arc::new(move || !stalled());
    (ready, live)
}

#[cfg(test)]
mod tests;
