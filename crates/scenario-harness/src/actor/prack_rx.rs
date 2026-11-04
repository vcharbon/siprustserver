//! A PRACK this callee receives (RFC 3262 §3): it acknowledges a reliable
//! provisional still unacknowledged — on a leg still up, or one whose INVITE
//! took its final meanwhile — or it names nothing and is answered 481 (RFC
//! 3261 §12.2.2). Answered or refused, it arrived: its CSeq is no hole in the
//! dialog's sequence.

use sip_message::header::RAck;
use sip_message::SipRequest;
use tokio::time::Instant;

use super::answer::answer_initial_invite;
use super::react::maybe_answer_held_invite;
use super::runner::ActorState;
use super::state::{LegPhase, Observation};
use crate::{ServerTxn, StepError};

/// A PRACK for our reliable 183: 200 it, then — MUST-014 — answer 200 to the
/// HELD INVITE transaction (no 200-to-INVITE before the PRACK). A FORKING
/// callee answers only on the WINNER fork's PRACK; a losing fork's PRACK is
/// 200'd and absorbed.
pub(super) async fn react_prack(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
) -> Result<(), StepError> {
    let now = Instant::now();
    let arrived = Observation::InDialogRequest {
        leg: st.role,
        call_id: uas.request().call_id().to_string(),
        cseq: uas.request().cseq().seq(),
        method: uas.request().method().as_str().to_string(),
    };
    if names_nothing(st, uas.request()) {
        uas.respond(481, "Call/Transaction Does Not Exist").try_send().await?;
        st.obs.record(arrived, now);
        return Ok(());
    }
    if after_the_leg(st) {
        uas.respond(200, "OK").try_send().await?;
        st.obs.record(arrived, now);
        return Ok(());
    }
    let prack_tag = uas.request().to().tag().map(str::to_owned);
    st.ctx.anchor(&st.agent, "prack", uas.request());
    uas.respond(200, "OK").try_send().await?;
    if let Some(tag) = &prack_tag {
        st.unpracked.remove(tag);
    }
    st.obs.record(arrived, now);
    // C5: this callee holds the INVITE for an early UPDATE — mark the 183
    // PRACKed and release the held 200 only once the UPDATE is also done
    // (MUST-014 + RFC 3311 §5.1, in either arrival order).
    if st.hold_for_early_update {
        st.early_pracked = true;
        return maybe_answer_held_invite(st).await;
    }
    let releases_answer = match (&mut st.fork_answer, prack_tag.as_deref()) {
        // Forking: the winner fork's PRACK (or, for an unrung winner, the last
        // fork's PRACK) releases the 200.
        (Some(f), Some(tag)) => f.releases_on_prack(tag),
        (Some(_), None) => false,
        // Non-forking reliable answer: any PRACK releases.
        (None, _) => true,
    };
    if releases_answer {
        if let Some(inv_txn) = st.pending_prack_answer.take() {
            let fork = st.fork_answer.take();
            answer_initial_invite(st, *inv_txn, fork).await?;
        }
    }
    Ok(())
}

/// Whether `prack` names nothing unacknowledged: the provisional its `RAck`
/// names on its dialog was PRACKed before. A PRACK that names something is
/// recorded as acknowledging it.
fn names_nothing(st: &mut ActorState, prack: &SipRequest) -> bool {
    let tag = prack.to().tag().unwrap_or_default().to_string();
    let rseq = prack.header::<RAck>().and_then(Result::ok).map(|r| r.rseq());
    rseq.is_some_and(|rseq| !st.pracked_rseqs.insert((tag, rseq)))
}

/// Whether this callee's leg ended before the PRACK arrived: the PRACK still
/// acknowledges its provisional, and moves nothing else.
fn after_the_leg(st: &ActorState) -> bool {
    st.obs.with_snapshot(|s| s.leg(st.role).phase() == LegPhase::Terminated)
}
