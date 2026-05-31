//! [`WorkerRegistryControl`] — the health-write seam (port of
//! `health/WorkerRegistryControl.ts`). The HealthProbe writes worker health
//! through this seam so it does not depend on a concrete registry's internals.
//! Three impls: an adapter over the [`SimulatedWorkerRegistry`] (tests), an
//! adapter over the shared [`RegistryState`] (the static registry's
//! production-wired health write seam, fed by the OPTIONS `HealthProbe`), and a
//! no-op (used where health must stay pinned).

use std::sync::Arc;

use sip_clock::Clock;

use super::simulated::SimulatedWorkerRegistry;
use super::{RegistryState, WorkerHealth};

/// The write seam. Suspending allowed (the routing hot path never calls this).
pub trait WorkerRegistryControl: Send + Sync {
    /// Annotate a worker's health. No-op if the worker is unknown or unchanged.
    fn set_health(&self, id: &str, health: WorkerHealth);
}

/// Adapter exposing a [`SimulatedWorkerRegistry`] as a control seam.
pub struct SimulatedControl {
    registry: SimulatedWorkerRegistry,
}

impl SimulatedControl {
    pub fn new(registry: SimulatedWorkerRegistry) -> Self {
        Self { registry }
    }
}

impl WorkerRegistryControl for SimulatedControl {
    fn set_health(&self, id: &str, health: WorkerHealth) {
        self.registry.set_health(id, health);
    }
}

/// Adapter exposing the shared [`RegistryState`] as a control seam. This is the
/// production wiring for the static registry: the OPTIONS [`HealthProbe`] writes
/// observed worker health straight into the same lock-free state the routing hot
/// path reads, so a worker that stops answering OPTIONS is demoted to `Dead` and
/// in-dialog requests fail over to the backup. The `Clock` stamps `draining_since`.
pub struct RegistryStateControl {
    state: Arc<RegistryState>,
    clock: Clock,
}

impl RegistryStateControl {
    pub(crate) fn new(state: Arc<RegistryState>, clock: Clock) -> Self {
        Self { state, clock }
    }
}

impl WorkerRegistryControl for RegistryStateControl {
    fn set_health(&self, id: &str, health: WorkerHealth) {
        let now = self.clock.now_ms().max(0) as u64;
        self.state.set_health(id, health, now);
    }
}

/// Production stub: accepts and discards all writes.
#[derive(Debug, Default, Clone)]
pub struct NoopControl;

impl WorkerRegistryControl for NoopControl {
    fn set_health(&self, _id: &str, _health: WorkerHealth) {}
}
