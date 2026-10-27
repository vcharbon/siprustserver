//! A clean end by the system under test, while the call accepts one
//! ([`CallCtx::accept_clean_ends`](crate::realcall::CallCtx::accept_clean_ends)):
//! the party drops her remaining goals. Once the caller is ended cleanly the
//! call is: its phase barriers are moot, and so are the goals of every party
//! holding no live dialog.

use sip_message::header::{Event, SubscriptionState};
use sip_message::SipRequest;
use tokio::time::Instant;

use super::endpoint::SUBFLOW_REFER;
use super::runner::ActorState;
use super::state::Observation;

/// Whether `notify` ends a subscription this party holds: it reports
/// `Subscription-State: terminated` (RFC 6665 §4.1.3) on the implicit
/// subscription of a REFER she sent and saw accepted (RFC 3515 §2.4.4).
pub(super) fn ends_own_subscription(st: &ActorState<'_>, notify: &SipRequest) -> bool {
    let terminated = notify
        .header::<SubscriptionState>()
        .and_then(Result::ok)
        .is_some_and(|s| s.token().eq_ignore_ascii_case("terminated"));
    let refer = notify
        .header::<Event>()
        .and_then(Result::ok)
        .is_some_and(|e| e.token().eq_ignore_ascii_case("refer"));
    terminated && refer && st.obs.with_snapshot(|s| s.leg(st.role).subflow(SUBFLOW_REFER).is_some())
}

/// The system under test ended this party cleanly — a non-2xx final to her
/// establishing INVITE, a BYE she answered 200, a NOTIFY ending her
/// subscription ([`ends_own_subscription`]). Returns whether the call
/// accepts it: her goals are then dropped, and an action among them is
/// recorded dropped.
pub(super) fn end_cleanly(st: &mut ActorState<'_>, now: Instant) -> bool {
    if !st.ctx.clean_ends_accepted() {
        return false;
    }
    st.obs.record(Observation::EndedCleanly { caller: st.originates }, now);
    if st.goals.remaining_steps().any(|step| !step.is_reception()) {
        st.obs.record(Observation::GoalDropped, now);
    }
    st.goals.abandon();
    true
}
