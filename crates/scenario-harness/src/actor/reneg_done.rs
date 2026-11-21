//! A renegotiation this leg originated finished: its 2xx ACKed
//! ([`complete_originated_reinvite`]) or refused with no retry to follow
//! ([`refused_reneg`]). Either one counts toward the leg's renegotiations.

use sip_message::SipResponse;
use tokio::time::Instant;

use super::endpoint::SUBFLOW_RENEG;
use super::ledger::{ObligationKey, ObligationKind};
use super::runner::ActorState;
use super::state::{Observation, SubflowState};

/// A renegotiation this leg originated drew a non-2xx final it does not
/// retry: it finished with the session unchanged (RFC 3261 §14.1), so it
/// counts toward [`LegObservation::reneg_count`](super::state::LegObservation::reneg_count)
/// and a script's next cycle follows.
pub(super) fn refused_reneg(st: &ActorState<'_>, resp: &SipResponse, now: Instant) {
    let tag = |t: Option<&str>| t.unwrap_or_default().to_string();
    let dialog = (tag(resp.from().tag()), tag(resp.to().tag()));
    let cseq = resp.cseq().seq();
    st.obs.record(Observation::RenegCompleted { leg: st.role, dialog, cseq }, now);
}

/// Close the bookkeeping of an originated re-INVITE once its 2xx is ACKed —
/// the `ReInvite` obligation, the `reneg` sub-flow, the completed-cycle count
/// (an N-cycle script's per-cycle barrier, C6) and the declared feed. ONCE per
/// CSeq (the `sent_reinvites` guard), so a re-emitted 2xx cannot double-count.
pub(super) fn complete_originated_reinvite(st: &mut ActorState<'_>, cseq: u32, now: Instant) {
    if st.sent_reinvites.remove(&cseq) {
        st.reinvite_retries = 0;
        st.sent_reinvite_txns.remove(&cseq);
        st.reinvite_ordinals.remove(&cseq);
        let key = ObligationKey::new(st.role, ObligationKind::ReInvite, cseq);
        st.obs.record(Observation::ResponseObserved { key }, now);
        st.obs.record(
            Observation::Subflow { leg: st.role, name: SUBFLOW_RENEG, to: SubflowState::Confirmed },
            now,
        );
        let dialog = st
            .dialogs
            .confirmed
            .as_ref()
            .map(|d| (d.local_tag().to_string(), d.remote_tag().to_string()))
            .unwrap_or_default();
        st.obs.record(Observation::RenegCompleted { leg: st.role, dialog, cseq }, now);
        st.feed.on_reinvite_ok.stamp(st.ctx);
    }
}
