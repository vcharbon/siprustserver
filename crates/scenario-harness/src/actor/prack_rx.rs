//! A PRACK this callee receives (RFC 3262 §3): it acknowledges a reliable
//! provisional still unacknowledged — on a leg still up, or one whose INVITE
//! took its final meanwhile — or it names nothing and is answered 481 (RFC
//! 3261 §12.2.2).

use sip_message::header::RAck;
use sip_message::SipRequest;

use super::runner::ActorState;
use super::state::LegPhase;

/// Whether `prack` names nothing unacknowledged: the provisional its `RAck`
/// names on its dialog was PRACKed before. A PRACK that names something is
/// recorded as acknowledging it.
pub(super) fn names_nothing(st: &mut ActorState, prack: &SipRequest) -> bool {
    let tag = prack.to().tag().unwrap_or_default().to_string();
    let rseq = prack.header::<RAck>().and_then(Result::ok).map(|r| r.rseq());
    rseq.is_some_and(|rseq| !st.pracked_rseqs.insert((tag, rseq)))
}

/// Whether this callee's leg ended before the PRACK arrived: the PRACK still
/// acknowledges its provisional, and moves nothing else.
pub(super) fn after_the_leg(st: &ActorState) -> bool {
    st.obs.with_snapshot(|s| s.leg(st.role).phase() == LegPhase::Terminated)
}
