//! [`WorkerRegistryControl`] — the health-write seam (port of
//! `health/WorkerRegistryControl.ts`). The HealthProbe writes worker health
//! through this seam so it does not depend on a concrete registry's internals.
//! Impls: an adapter over the shared [`WorkerSet`] (the production wiring, fed by
//! the OPTIONS `HealthProbe`), an adapter over the [`SimulatedWorkerRegistry`]
//! (tests), and a no-op (used where health must stay pinned).

use std::sync::Arc;

use super::projection::WorkerSet;
use super::simulated::SimulatedWorkerRegistry;
use super::WorkerHealth;

/// The write seam. Suspending allowed (the routing hot path never calls this).
pub trait WorkerRegistryControl: Send + Sync {
    /// Annotate a worker's health. No-op if the worker is unknown or unchanged.
    fn set_health(&self, id: &str, health: WorkerHealth);
}

/// Adapter exposing a shared [`WorkerSet`]'s health annotation as a control seam.
/// This is the production wiring: the OPTIONS [`HealthProbe`](crate::health::HealthProbe)
/// writes observed worker health into the same annotation overlay the projection
/// reads, so a worker that stops answering OPTIONS is demoted to `Dead` and
/// in-dialog requests fail over to the backup.
pub struct WorkerSetControl {
    set: Arc<WorkerSet>,
}

impl WorkerSetControl {
    pub(crate) fn new(set: Arc<WorkerSet>) -> Self {
        Self { set }
    }
}

impl WorkerRegistryControl for WorkerSetControl {
    fn set_health(&self, id: &str, health: WorkerHealth) {
        self.set.set_health(id, health);
    }
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

/// Production stub: accepts and discards all writes.
#[derive(Debug, Default, Clone)]
pub struct NoopControl;

impl WorkerRegistryControl for NoopControl {
    fn set_health(&self, _id: &str, _health: WorkerHealth) {}
}
