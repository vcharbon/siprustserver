//! The [`Barrier`] a goal waits on before it fires: a guard over the observed
//! multi-endpoint state.

use std::sync::Arc;

use super::state::{LegPhase, StateInner};

/// A barrier guard over the observed multi-endpoint state.
#[derive(Clone)]
pub enum Barrier {
    /// Fire immediately (no guard).
    None,
    /// Fire once every named leg is at least `Confirmed` — the `established`
    /// guard (bob's REFER / alice's BYE wait on it).
    AllConfirmed(&'static [&'static str]),
    /// Fire once a named predicate over the observed state holds — the open
    /// form (the refer `merged` conjunction is `Pred`). The name is the bounded
    /// label a guard-timeout `StepError::who` carries (never free-form).
    Pred { name: &'static str, pred: Arc<dyn Fn(&StateInner) -> bool + Send + Sync> },
}

impl Barrier {
    /// A [`Barrier::Pred`] from a name + predicate (the ergonomic constructor).
    pub fn pred(
        name: &'static str,
        pred: impl Fn(&StateInner) -> bool + Send + Sync + 'static,
    ) -> Self {
        Barrier::Pred { name, pred: Arc::new(pred) }
    }

    /// A barrier that holds once `leg` has received an inbound in-dialog request
    /// of `method` (the RFC name, e.g. `"INFO"`) — the observed-fact gate that
    /// orders an origination AFTER a specific inbound request (an MRF's
    /// `INFO(EOF)` following the worker's `INFO(play)`), replacing a timed
    /// post-confirm dwell. `name` is the bounded barrier label a guard-timeout
    /// [`StepError::who`](crate::StepError) carries (never free-form).
    pub fn received(name: &'static str, leg: &'static str, method: &'static str) -> Self {
        Barrier::pred(name, move |s| s.leg_received_method(leg, method))
    }

    /// Whether the guard holds against a state snapshot.
    pub fn holds(&self, s: &StateInner) -> bool {
        match self {
            Barrier::None => true,
            Barrier::AllConfirmed(roles) => {
                roles.iter().all(|r| s.leg_at_least(r, LegPhase::Confirmed))
            }
            Barrier::Pred { pred, .. } => pred(s),
        }
    }

    /// A bounded label for the barrier-timeout `StepError::who` (never free-form).
    pub fn name(&self) -> &'static str {
        match self {
            Barrier::None => "none",
            Barrier::AllConfirmed(_) => "established",
            Barrier::Pred { name, .. } => name,
        }
    }
}

impl std::fmt::Debug for Barrier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Barrier::None => f.write_str("Barrier::None"),
            Barrier::AllConfirmed(roles) => write!(f, "Barrier::AllConfirmed({roles:?})"),
            Barrier::Pred { name, .. } => write!(f, "Barrier::Pred({name:?})"),
        }
    }
}
