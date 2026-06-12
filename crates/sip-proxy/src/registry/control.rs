//! [`WorkerRegistryControl`] — the health-write seam (port of
//! `health/WorkerRegistryControl.ts`). The HealthProbe writes worker health
//! through this seam so it does not depend on a concrete registry's internals.
//! The one impl is the adapter over the shared [`WorkerSet`] — every registry
//! (static, composed, simulated) wraps a `WorkerSet`, so each hands out the
//! same adapter from its `control()`.

use std::sync::Arc;

use super::projection::WorkerSet;
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

