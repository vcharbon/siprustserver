//! [`BackendGate`] — the backends whose breakers gate a node's acceptance of
//! new calls (ADR-0043 §11, ADR-0050 §9): while any of them is open, a node
//! otherwise `Ready` reports `new=refuse; cause=backend` on its OPTIONS `200`,
//! and `new=accept` again once every one has closed. Never latched, and never
//! read by the orchestrator's `/ready` nor by in-dialog routing.

use std::sync::{PoisonError, RwLock};

use tokio::sync::watch;

/// The breakers gating the acceptance of new calls, each `true` while open.
#[derive(Default)]
pub(crate) struct BackendGate {
    breakers: RwLock<Vec<watch::Receiver<bool>>>,
}

impl BackendGate {
    /// From now on, `breaker` open refuses new calls.
    pub(crate) fn add(&self, breaker: watch::Receiver<bool>) {
        self.breakers.write().unwrap_or_else(PoisonError::into_inner).push(breaker);
    }

    /// Whether any breaker gating it is open now.
    pub(crate) fn any_open(&self) -> bool {
        self.breakers.read().unwrap_or_else(PoisonError::into_inner).iter().any(|b| *b.borrow())
    }
}
