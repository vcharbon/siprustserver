//! [`WorkerRegistryControl`] — the health-write seam (port of
//! `health/WorkerRegistryControl.ts`). The HealthProbe writes worker health
//! through this seam so it does not depend on a concrete registry's internals.
//! Two impls: an adapter over the [`SimulatedWorkerRegistry`] (tests) and a
//! no-op (production, before the k8s watcher lands — the probe still logs, the
//! registry stays static).

use super::simulated::SimulatedWorkerRegistry;
use super::WorkerHealth;

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

/// Production stub: accepts and discards all writes.
#[derive(Debug, Default, Clone)]
pub struct NoopControl;

impl WorkerRegistryControl for NoopControl {
    fn set_health(&self, _id: &str, _health: WorkerHealth) {}
}
