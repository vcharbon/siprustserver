//! A 481 or 408 to a request on a party's confirmed dialog ends that dialog
//! (RFC 3261 §12.2.1.2; §15.1.1 for a BYE): no peer holds it. What the party
//! still awaited on it is moot, and so are its remaining goals.

use sip_message::SipResponse;
use tokio::time::Instant;

use super::answer::discharge_on_teardown;
use super::runner::ActorState;
use super::state::Observation;

/// End the party's confirmed dialog when `resp`, a final to one of its
/// in-dialog requests, says no peer holds it. Returns whether it did.
pub(super) fn end_refused_dialog(
    st: &mut ActorState<'_>,
    resp: &SipResponse,
    now: Instant,
) -> bool {
    let refused = matches!(resp.status(), 408 | 481);
    let on_confirmed = st
        .dialogs
        .confirmed
        .as_ref()
        .zip(resp.to().tag())
        .is_some_and(|(d, tag)| d.remote_tag() == tag);
    if !refused || !on_confirmed || matches!(resp.cseq().method().as_str(), "CANCEL" | "PRACK") {
        return false;
    }
    discharge_on_teardown(st, now);
    st.obs.record(Observation::LegTerminated { leg: st.role }, now);
    st.scope.mark_terminated();
    st.goals.abandon();
    true
}
