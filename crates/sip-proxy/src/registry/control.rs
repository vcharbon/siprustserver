//! [`WorkerRegistryControl`]: the health-write seam. The health probe writes
//! worker health through it, never through a concrete registry's internals.
//! Its one impl adapts the shared `WorkerSet` that every registry (static,
//! composed, simulated) wraps, so each hands out the same adapter from its
//! `control()`.

use std::sync::Arc;

use super::projection::WorkerSet;
use super::{Acceptance, WorkerHealth};

/// The write seam of worker health. The routing hot path never calls it.
pub trait WorkerRegistryControl: Send + Sync {
    /// Annotate a worker's health. No-op if the worker is unknown or unchanged.
    fn set_health(&self, id: &str, health: WorkerHealth);
    /// Annotate a worker's health and acceptance of new calls in one write
    /// (`None` keeps that one), so no reader sees one without the other.
    /// No-op if the worker is unknown or unchanged.
    fn set_view(&self, id: &str, health: Option<WorkerHealth>, acceptance: Option<Acceptance>);
}

/// Adapter exposing a shared `WorkerSet`'s health annotation as a control seam.
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
    fn set_view(&self, id: &str, health: Option<WorkerHealth>, acceptance: Option<Acceptance>) {
        self.set.set_view(id, health, acceptance);
    }
}
