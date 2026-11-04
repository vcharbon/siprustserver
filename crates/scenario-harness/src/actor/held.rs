//! The [`Disposition::Held`](super::Disposition::Held) callee: an initial
//! INVITE no policy answers. Its goals ring it ([`ring`]), send requests on
//! its early dialogs ([`callee_early_request`]) and answer or reject it
//! ([`answer_held`], [`reject_held`]); [`answer_ready`], [`reject_ready`] and
//! [`early_request_ready`] are the goal-arm gates that keep those steps RFC
//! 3262 compliant (the reject's gate is stricter than RFC 3262 asks).

use tokio::time::Instant;

use sip_message::generators::InDialogMethod;

use super::answer::{answer_initial_invite, reject_initial, ForkAnswer};
use super::endpoint::ForkWinner;
use super::ledger::{ObligationKey, ObligationKind};
use super::runner::ActorState;
use super::state::Observation;
use crate::{ServerTxn, StepError};

/// Hold a fresh initial INVITE for the goals: `100 Trying` at once, since
/// the goals may ring it after more than 200 ms (RFC 3261 §17.2.1).
pub(super) async fn hold_initial(
    st: &mut ActorState<'_>,
    mut uas: ServerTxn,
) -> Result<(), StepError> {
    uas.respond(100, "Trying").try_send().await?;
    st.obs.record(Observation::LegEarly { leg: st.role }, Instant::now());
    st.unpracked.clear();
    st.early_dialogs.clear();
    st.held = Some(Box::new(uas));
    Ok(())
}

/// The local tag a provisional under `fork` carries: the fork's tag, or the
/// transaction's own (minted by its first provisional).
fn early_tag(txn: &ServerTxn, fork: Option<&str>) -> String {
    match fork {
        Some(tag) => tag.to_string(),
        None => txn.dialog().local_tag().to_string(),
    }
}

/// `GoalStep::Ring`: one provisional per fork tag, or one under the
/// transaction's own tag; a reliable one awaits its PRACK.
pub(super) async fn ring(
    st: &mut ActorState<'_>,
    forks: &'static [&'static str],
    reliable: bool,
) -> Result<(), StepError> {
    let sdp = st.answer_body();
    let Some(txn) = st.held.as_mut() else {
        return Err(no_held_invite(st.role, "Ring"));
    };
    let tags: Vec<Option<&'static str>> =
        if forks.is_empty() { vec![None] } else { forks.iter().copied().map(Some).collect() };
    for fork in tags {
        let mut r = if reliable {
            txn.respond(183, "Session Progress").reliable(1).with_sdp(sdp)
        } else {
            txn.respond(180, "Ringing")
        };
        if let Some(tag) = fork {
            r = r.with_to_tag(tag);
        }
        r.try_send().await?;
        let tag = early_tag(txn, fork);
        st.early_provisionals.insert(fork.map(str::to_string).unwrap_or_default());
        if reliable {
            st.unpracked.entry(tag).or_insert_with(Instant::now);
        }
    }
    // A non-2xx final of a forked transaction is one rung fork's, as the
    // forking proxy forwards one branch's final (RFC 3261 §16.7): the first's.
    if let Some(first) = forks.first() {
        txn.adopt_to_tag(first);
    }
    st.obs.record(Observation::LegEarly { leg: st.role }, Instant::now());
    Ok(())
}

/// Whether `GoalStep::Ring` may fire: the INVITE it rings has arrived.
pub(super) fn ring_ready(st: &ActorState<'_>) -> bool {
    st.held.is_some()
}

/// Whether `GoalStep::Answer { winner }` may fire: the INVITE is held and the
/// answering early dialog's reliable provisional is PRACKed, every fork's for
/// an unrung winner.
pub(super) fn answer_ready(st: &ActorState<'_>, winner: Option<ForkWinner>) -> bool {
    if st.held.is_none() {
        return false;
    }
    match winner {
        Some(ForkWinner::Tag(tag)) => !st.unpracked.contains_key(tag),
        Some(ForkWinner::Unrung) | None => st.unpracked.is_empty(),
    }
}

/// `GoalStep::Answer`: `200` + SDP on the held INVITE under `winner`.
pub(super) async fn answer_held(
    st: &mut ActorState<'_>,
    winner: Option<ForkWinner>,
) -> Result<(), StepError> {
    let Some(txn) = st.held.take().map(|t| *t) else {
        return Err(no_held_invite(st.role, "Answer"));
    };
    let fork = winner.map(|winner| ForkAnswer {
        winner,
        loser_late_200: None,
        unpracked: Default::default(),
    });
    answer_initial_invite(st, txn, fork).await
}

/// Whether `GoalStep::Reject` may fire: the INVITE is held and every reliable
/// provisional it drew is PRACKed.
pub(super) fn reject_ready(st: &ActorState<'_>) -> bool {
    st.held.is_some() && st.unpracked.is_empty()
}

/// `GoalStep::Reject`: the non-2xx final `status` on the held INVITE, which
/// ends the leg and awaits its hop-ACK.
pub(super) async fn reject_held(st: &mut ActorState<'_>, status: u16) -> Result<(), StepError> {
    if status < 300 {
        return Err(StepError::UnexpectedKind {
            who: st.role.to_string(),
            detail: format!("Reject goal with the non-final-error status {status}"),
        });
    }
    let Some(txn) = st.held.take().map(|t| *t) else {
        return Err(no_held_invite(st.role, "Reject"));
    };
    reject_initial(st, txn, status).await
}

/// Whether a callee's `GoalStep::EarlyRequest` on `fork` may fire: the
/// INVITE is held and that early dialog's reliable provisional is PRACKed,
/// so the caller holds the dialog (RFC 3262 §3, RFC 3311 §5.1).
pub(super) fn early_request_ready(st: &ActorState<'_>, fork: Option<&str>) -> bool {
    st.held.as_ref().is_some_and(|txn| !st.unpracked.contains_key(&early_tag(txn, fork)))
}

/// A callee's request on the early dialog of `fork` of its held INVITE. The
/// dialog is formed once per tag and kept, so its CSeq runs on across
/// requests and into the dialog the 2xx confirms (RFC 3261 §12.2.1.1).
pub(super) async fn callee_early_request(
    st: &mut ActorState<'_>,
    method: InDialogMethod,
    fork: Option<&'static str>,
) -> Result<(), StepError> {
    let now = Instant::now();
    let offer = st.media.reoffer_sdp().unwrap_or(crate::ANSWER_SDP);
    let Some(txn) = st.held.as_mut() else {
        return Err(no_held_invite(st.role, "EarlyRequest"));
    };
    let tag = early_tag(txn, fork);
    let dialog = st.early_dialogs.entry(tag.clone()).or_insert_with(|| {
        if fork.is_some() {
            txn.adopt_to_tag(&tag);
        }
        txn.dialog()
    });
    let mut req = dialog.send_request(method);
    if method == InDialogMethod::Update {
        req = req.with_sdp(offer);
    }
    let (_txn, request) = req.try_send_with_request().await?;
    let cseq = request.cseq().seq();
    let kind =
        ObligationKind::from_cseq_method(method.as_str()).unwrap_or(ObligationKind::InDialog);
    if method == InDialogMethod::Update {
        st.sent_updates.insert(cseq);
    }
    st.obs.record(
        Observation::RequestSent {
            key: ObligationKey::new(st.role, kind, cseq),
            detail: format!("early {} awaiting 2xx", method.as_str()),
        },
        now,
    );
    Ok(())
}

/// The fail-fast error of a held-INVITE goal with no held INVITE.
fn no_held_invite(role: &str, step: &str) -> StepError {
    StepError::UnexpectedKind {
        who: role.to_string(),
        detail: format!("{step} goal with no held initial INVITE"),
    }
}
