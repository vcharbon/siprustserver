//! A provisional lost to a crash: an unreliable provisional (an 18x without
//! RFC 3262) is not guaranteed across the crash of a node that relays it, so
//! a caller may go from silence straight to her final. While the call
//! accepts that ([`CallCtx::accept_lost_provisionals`]), a caller whose
//! establishing INVITE reaches its final with no provisional before it
//! records the loss, and a goal or phase waiting for her to ring reads it as
//! rung ([`provisional_lost`]). Without the acceptance nothing is recorded:
//! a missing provisional stays a failure.
//!
//! [`CallCtx::accept_lost_provisionals`]: crate::realcall::CallCtx::accept_lost_provisionals

use tokio::time::Instant;

use super::runner::ActorState;
use super::state::{Observation, StateInner, SubflowState};

/// The sub-flow that records a provisional lost before the final.
const SUBFLOW_PROVISIONAL_LOST: &str = "provisional-lost";

/// This party's establishing INVITE reached its final: record a provisional
/// lost before it, when none came and the call accepts the loss.
pub(super) fn note_final(st: &ActorState<'_>, now: Instant) {
    if !st.saw_provisional && st.ctx.lost_provisionals_accepted() {
        let (leg, name, to) = (st.role, SUBFLOW_PROVISIONAL_LOST, SubflowState::Answered);
        st.obs.record(Observation::Subflow { leg, name, to }, now);
    }
}

/// Whether `role`'s establishing INVITE reached its final with no provisional
/// before it, in a call that accepts a provisional lost to a crash.
pub fn provisional_lost(s: &StateInner, role: &str) -> bool {
    s.leg(role).subflow(SUBFLOW_PROVISIONAL_LOST).is_some()
}
