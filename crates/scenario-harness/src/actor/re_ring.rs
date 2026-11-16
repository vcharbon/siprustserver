//! A ringing callee rings again when the loss model dropped an unreliable
//! provisional it sent: RFC 3261 §13.3.1.1 lets a UAS send further
//! provisionals, and a system under test that never learns the callee rings
//! cannot arm the no-answer timer its first 18x starts. A reliable
//! provisional is the retransmit engine's to re-send.

use std::time::Duration;

use tokio::time::Instant;

use super::barrier::await_pred;
use super::runner::ActorState;
use super::state::{ObservedState, StateInner};
use crate::realcall::LostMessage;
use crate::StepError;

/// The unreliable provisionals to its INVITE that `role` sent and the loss
/// model dropped, in the order dropped.
fn lost_rings(s: &StateInner, role: &str) -> Vec<LostMessage> {
    let ring = |m: &&LostMessage| !m.inbound && !m.reliable && m.is_invite_response_in(101..200);
    s.leg_ref(role).map_or_else(Vec::new, |l| l.lost().filter(ring).cloned().collect())
}

/// Whether the actor holds an INVITE it has not answered: one it may ring.
pub(super) fn armed(st: &ActorState<'_>) -> bool {
    st.held_silent.is_some() || st.pending_answer.is_some() || st.held.is_some()
}

/// Until a ring of `role` beyond the first `rung` is lost.
pub(super) async fn wait(obs: &ObservedState, role: &'static str, rung: usize) {
    let far = Instant::now() + Duration::from_secs(3600);
    let _ = await_pred(obs, "re_ring", move |s| lost_rings(s, role).len() > rung, far).await;
}

/// Send each lost ring again on the held INVITE, under the tag it carried.
pub(super) async fn fire(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let lost: Vec<LostMessage> =
        st.obs.with_snapshot(|s| lost_rings(s, st.role).into_iter().skip(st.rerung).collect());
    let sdp = st.answer_body();
    for ring in &lost {
        let txn = st
            .held_silent
            .as_deref_mut()
            .or(st.pending_answer.as_deref_mut().map(|ta| &mut ta.uas))
            .or(st.held.as_deref_mut());
        let Some(txn) = txn else { break };
        let mut r = txn.respond(ring.status.unwrap_or(180), "Ringing");
        if !ring.to_tag.is_empty() {
            r = r.with_to_tag(&ring.to_tag);
        }
        if ring.sdp {
            r = r.with_sdp(sdp);
        }
        r.try_send().await?;
    }
    st.rerung += lost.len();
    Ok(())
}
