//! [`BackendGate`] — the backends whose breakers gate the SIP OPTIONS
//! self-health (ADR-0043): while any of them is open, a node otherwise
//! `Ready` answers OPTIONS `503` not-ready, and `200` again once every one
//! has closed. Never latched, and never read by the orchestrator's `/ready`:
//! a backend outage across the cluster steers new dialogs away through the
//! front proxy without emptying the node's service membership.

use std::sync::{PoisonError, RwLock};

use tokio::sync::watch;

/// The breakers gating the OPTIONS self-health, each `true` while open.
#[derive(Default)]
pub(crate) struct BackendGate {
    breakers: RwLock<Vec<watch::Receiver<bool>>>,
}

impl BackendGate {
    /// From now on, `breaker` open gates the OPTIONS self-health.
    pub(crate) fn add(&self, breaker: watch::Receiver<bool>) {
        self.breakers.write().unwrap_or_else(PoisonError::into_inner).push(breaker);
    }

    /// Whether any breaker gating it is open now.
    pub(crate) fn any_open(&self) -> bool {
        self.breakers.read().unwrap_or_else(PoisonError::into_inner).iter().any(|b| *b.borrow())
    }
}
