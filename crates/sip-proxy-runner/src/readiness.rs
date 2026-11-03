//! The proxy's probe answers (ADR-0012 D4, ADR-0046 D4, ADR-0050 §11,
//! ADR-0056): `/readyz` is fit for traffic once the first worker view is in,
//! with at least one `Alive` worker whatever its acceptance of new calls,
//! every recv shard turning, the external face usable, and no drain latched.
//! The node announces the proxy's addresses exactly while `/readyz` passes.
//! `/healthz` is live while every recv shard turns.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use probe_http::{LiveFn, ProbeState, ReadyFn};
use sip_clock::Clock;
use sip_proxy::health::FirstView;
use sip_proxy::liveness::ShardPulse;
use sip_proxy::registry::WorkerRegistry;

use crate::external_face::FaceCheck;

/// Draining once SIGTERM latched; else NotReady while a recv shard is
/// stalled, before the first worker view, with the external face unusable,
/// or with no worker `Alive`; else Ready.
pub fn probe_state(
    draining: bool,
    shard_stalled: bool,
    first_view: bool,
    external_face_usable: bool,
    registry: &dyn WorkerRegistry,
) -> ProbeState {
    if draining {
        ProbeState::Draining
    } else if shard_stalled || !first_view || !external_face_usable {
        ProbeState::NotReady
    } else if registry.snapshot().iter().any(|w| w.is_alive()) {
        ProbeState::Ready
    } else {
        ProbeState::NotReady
    }
}

/// What the probe server answers with: `/readyz` and `/healthz`.
pub struct ProxyProbes {
    pub ready: ReadyFn,
    pub live: LiveFn,
}

/// The probes, read from `pulse` on `clock`, from `first_view` and from
/// `external_face` (none: no face check): a shard holding one packet longer
/// than `stall_ms` fails both, so the node withdraws the addresses and the
/// livenessProbe restarts the process. A shard waiting on an empty socket is
/// never stalled. The external face is read from the kernel on each
/// `/readyz`.
pub fn probes(
    draining: Arc<AtomicBool>,
    pulse: Arc<ShardPulse>,
    clock: Clock,
    stall_ms: u64,
    registry: Arc<dyn WorkerRegistry>,
    first_view: Arc<FirstView>,
    external_face: Option<FaceCheck>,
) -> ProxyProbes {
    let stalled = {
        let pulse = pulse.clone();
        let clock = clock.clone();
        move || !pulse.stalled(u64::try_from(clock.now_ms()).unwrap_or(0), stall_ms).is_empty()
    };
    let ready_stalled = stalled.clone();
    let ready_view = first_view;
    let ready: ReadyFn = Arc::new(move || {
        probe_state(
            draining.load(Ordering::Relaxed),
            ready_stalled(),
            ready_view.is_in(),
            external_face.as_ref().is_none_or(|face| face.read().is_usable()),
            registry.as_ref(),
        )
    });
    let live: LiveFn = Arc::new(move || !stalled());
    ProxyProbes { ready, live }
}

#[cfg(test)]
mod tests;
