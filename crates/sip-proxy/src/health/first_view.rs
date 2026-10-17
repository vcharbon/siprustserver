//! [`FirstView`] — the proxy's first worker view (ADR-0050 §5, §11): the
//! worker registry holds its first authoritative listing and one probe round
//! over it has completed, each listed worker having answered or run out its
//! reply window. Latched once by the [`HealthProbe`](super::HealthProbe),
//! never cleared; read by the routing core and the runner's probe routes.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Whether the proxy has its first worker view. Before it, a request with no
/// target is dropped unanswered and the proxy is not ready.
#[derive(Debug, Default)]
pub struct FirstView(AtomicBool);

impl FirstView {
    /// A view still to come: a starting proxy's, latched by its probe.
    pub fn pending() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(false)))
    }

    /// A view already given: a proxy whose worker health is set by its
    /// builder rather than probed.
    pub fn given() -> Arc<Self> {
        Arc::new(Self(AtomicBool::new(true)))
    }

    /// Whether the first view is in.
    pub fn is_in(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Latch the first view. Idempotent.
    pub fn latch(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// The probe's first round: the branches it fanned out, until each has left
/// the in-flight set (answered, retired or reaped) or the round's reply window
/// has run out.
#[derive(Debug, Default)]
pub(crate) struct FirstRound {
    branches: HashSet<String>,
}

impl FirstRound {
    /// The round of `branches`, the probes just sent.
    pub(crate) fn of(branches: impl IntoIterator<Item = String>) -> Self {
        Self { branches: branches.into_iter().collect() }
    }

    /// Whether every probe of the round is settled, `in_flight` telling
    /// whether a branch still awaits its answer.
    pub(crate) fn settled(&self, in_flight: impl Fn(&str) -> bool) -> bool {
        self.branches.iter().all(|b| !in_flight(b))
    }
}
