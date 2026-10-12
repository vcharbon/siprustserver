//! The proxy's `/readyz` state (ADR-0012 D4): fit for traffic only with at
//! least one `Alive` worker, every recv shard turning, and no drain latched.

use probe_http::ProbeState;
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
