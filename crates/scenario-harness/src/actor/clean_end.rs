//! A clean end by the system under test, while the call accepts one
//! ([`CallCtx::accept_clean_ends`](crate::realcall::CallCtx::accept_clean_ends)):
//! the party drops her remaining goals. Once the caller is ended cleanly the
//! call is: its phase barriers are moot, and so are the goals of every party
//! holding no live dialog.

use tokio::time::Instant;

use super::runner::ActorState;
use super::state::Observation;

/// The system under test ended this party cleanly — a non-2xx final to her
/// establishing INVITE, a BYE she answered 200. Returns whether the call
/// accepts it: her goals are then dropped.
pub(super) fn end_cleanly(st: &mut ActorState<'_>, now: Instant) -> bool {
    if !st.ctx.clean_ends_accepted() {
        return false;
    }
    st.obs.record(Observation::EndedCleanly { caller: st.originates }, now);
    st.goals.abandon();
    true
}
