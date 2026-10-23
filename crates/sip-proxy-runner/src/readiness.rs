//! The proxy's probe answers (ADR-0012 D4, ADR-0046 D4, ADR-0050 §11):
//! `/readyz` is fit for traffic once the first worker view is in, with at
//! least one `Alive` worker whatever its acceptance of new calls, every recv
//! shard turning, and no drain latched; `/healthz` is live while every recv
//! shard turns; `/vrrp`, the virtual-address check, passes while `/healthz`
//! does and the first worker view is in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use probe_http::{LiveFn, ProbeState, ReadyFn};
use sip_clock::Clock;
use sip_proxy::health::FirstView;
use sip_proxy::liveness::ShardPulse;
use sip_proxy::registry::WorkerRegistry;

/// Draining once SIGTERM latched; else NotReady while a recv shard is
/// stalled, before the first worker view, or with no worker `Alive`; else
/// Ready.
pub fn probe_state(
    draining: bool,
    shard_stalled: bool,
    first_view: bool,
    registry: &dyn WorkerRegistry,
) -> ProbeState {
    if draining {
        ProbeState::Draining
    } else if shard_stalled || !first_view {
        ProbeState::NotReady
    } else if registry.snapshot().iter().any(|w| w.is_alive()) {
        ProbeState::Ready
    } else {
        ProbeState::NotReady
    }
}

/// What the probe server answers with: `/readyz`, `/healthz`, and the
/// condition `/vrrp` adds to `/healthz`.
pub struct ProxyProbes {
    pub ready: ReadyFn,
    pub live: LiveFn,
    pub vrrp: LiveFn,
}

/// The probes, read from `pulse` on `clock` and from `first_view`: a shard
/// holding one packet longer than `stall_ms` fails all three, so the VIP's
/// check moves the address and the livenessProbe restarts the process. A
/// shard waiting on an empty socket is never stalled.
pub fn probes(
    draining: Arc<AtomicBool>,
    pulse: Arc<ShardPulse>,
    clock: Clock,
    stall_ms: u64,
    registry: Arc<dyn WorkerRegistry>,
    first_view: Arc<FirstView>,
) -> ProxyProbes {
    let stalled = {
        let pulse = pulse.clone();
        let clock = clock.clone();
        move || !pulse.stalled(u64::try_from(clock.now_ms()).unwrap_or(0), stall_ms).is_empty()
    };
    let ready_stalled = stalled.clone();
    let ready_view = first_view.clone();
    let ready: ReadyFn = Arc::new(move || {
        probe_state(
            draining.load(Ordering::Relaxed),
            ready_stalled(),
            ready_view.is_in(),
            registry.as_ref(),
        )
    });
    let live: LiveFn = Arc::new(move || !stalled());
    let vrrp: LiveFn = Arc::new(move || first_view.is_in());
    ProxyProbes { ready, live, vrrp }
}

#[cfg(test)]
mod tests;
