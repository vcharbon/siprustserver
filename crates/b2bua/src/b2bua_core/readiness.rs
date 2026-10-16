//! The worker's readiness as its host and harnesses read it: the handle, the
//! `/ready` gate and the state behind it.

use super::B2buaCore;
use crate::repl::{Readiness, ReadinessState};

impl B2buaCore {
    /// This core's readiness handle (clone-cheap, shares the latches). Exposed
    /// so a harness can observe the node's own drain/readiness state after the
    /// core itself is no longer reachable.
    pub fn readiness(&self) -> Readiness {
        self.readiness.clone()
    }

    /// Readiness gate, routed through the SAME latched [`Readiness`] state
    /// machine the SIP OPTIONS self-report uses (X6 anti-flap). With ready-gated
    /// EndpointSlice membership, an un-latched predicate would flip back to 503
    /// on a desired-but-not-yet-current peer and unpublish the node, and a
    /// simultaneously-restarted cluster would oscillate published↔unpublished.
    /// The latch makes Ready sticky on both probe surfaces; `Draining` reports
    /// not-ready (the probe's job during drain is to unpublish).
    pub fn is_ready(&self) -> bool {
        self.readiness.state() == ReadinessState::Ready
    }

    /// The full 3-valued readiness state (`NotReady`/`Ready`/`Draining`). The
    /// runner maps it onto the HTTP probe's `ProbeState` so `/ready` can report
    /// `draining` distinctly from `not-ready`.
    pub fn readiness_state(&self) -> ReadinessState {
        self.readiness.state()
    }
}
