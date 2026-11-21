//! The PRACKs a caller holds back ([`Answers::prack`](super::answers::Answers)):
//! each reliable provisional is acknowledged a drawn time after it arrived,
//! while its sender re-sends it (RFC 3262 §3). A PRACK that comes due after
//! the INVITE's 2xx still goes out, on the dialog the 2xx confirmed or on the
//! sibling fork's early dialog, which lives on for 64·T1 (RFC 3261
//! §13.2.2.4); one whose INVITE ended with no dialog is dropped.

use sip_message::generators::InDialogMethod;
use sip_message::SipResponse;
use tokio::time::Instant;

use super::endpoint::SUBFLOW_EARLY;
use super::ledger::{ObligationKey, ObligationKind};
use super::runner::ActorState;
use super::state::{Observation, SubflowState};
use crate::agent::rack_for;
use crate::StepError;

/// A PRACK not yet sent.
pub(super) struct HeldPrack {
    pub(super) at: Instant,
    resp: SipResponse,
    answer: Option<(String, Vec<u8>)>,
}

/// Hold the PRACK of `resp`, carrying `answer`, for the endpoint's drawn time.
pub(super) fn hold(st: &mut ActorState<'_>, resp: SipResponse, answer: Option<(String, Vec<u8>)>) {
    let at = Instant::now() + st.holds.answers.prack;
    st.holds.pracks.push(HeldPrack { at, resp, answer });
}

/// When the earliest held PRACK is due.
pub(super) fn due(st: &ActorState<'_>) -> Option<Instant> {
    st.holds.pracks.iter().map(|h| h.at).min()
}

/// A final to the INVITE arrived: send at once every held PRACK it would
/// otherwise overtake, the ones of its early dialog for a 2xx, all of them for
/// a non-2xx final, while the INVITE is still pending (RFC 3262 §4: the
/// PRACK precedes the UAC's taking of the final). A sibling fork's stays held.
pub(super) async fn before_final(
    st: &mut ActorState<'_>,
    fin: &SipResponse,
) -> Result<(), StepError> {
    if st.holds.pracks.is_empty() || st.dialogs.pending_invite.is_none() {
        return Ok(());
    }
    let tag = fin.to().tag().unwrap_or_default().to_string();
    let all = fin.status() >= 300;
    let (now, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut st.holds.pracks)
        .into_iter()
        .partition(|h| all || h.resp.to().tag().unwrap_or_default() == tag);
    st.holds.pracks = kept;
    for held in now {
        send(st, held).await?;
    }
    Ok(())
}

/// Send every held PRACK due by `until`.
pub(super) async fn fire(st: &mut ActorState<'_>, until: Option<Instant>) -> Result<(), StepError> {
    let (due, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut st.holds.pracks)
        .into_iter()
        .partition(|h| until.is_none_or(|t| h.at <= t));
    st.holds.pracks = kept;
    for held in due {
        send(st, held).await?;
    }
    Ok(())
}

/// Send one PRACK on whichever dialog its provisional's early dialog became:
/// the confirmed dialog itself, so its CSeq runs on, or a sibling fork's early
/// dialog derived from the won INVITE.
async fn send(st: &mut ActorState<'_>, held: HeldPrack) -> Result<(), StepError> {
    let HeldPrack { resp, answer, .. } = held;
    let request = if let Some(inv) = st.dialogs.pending_invite.as_mut() {
        inv.try_prack_carrying(&resp, answer).await?.1
    } else {
        let Some(rack) = rack_for(&resp) else { return Ok(()) };
        let tag = resp.to().tag().unwrap_or_default();
        let on_confirmed = st.dialogs.confirmed.as_ref().is_some_and(|d| d.remote_tag() == tag);
        let mut fork = None;
        if !on_confirmed {
            let Some(won) = st.dialogs.won_invite.as_ref() else { return Ok(()) };
            fork = Some(won.fork_dialog(&st.agent, &resp));
        }
        let dialog = match fork.as_mut() {
            Some(f) => f,
            None => match st.dialogs.confirmed.as_mut() {
                Some(d) => d,
                None => return Ok(()),
            },
        };
        let mut req = dialog.send_request(InDialogMethod::Prack).with_rack(&rack);
        if let Some((content_type, bytes)) = answer {
            req = req.with_body(&content_type, bytes);
        }
        req.try_send_with_request().await?.1
    };
    let now = Instant::now();
    st.obs.record(
        Observation::RequestSent {
            key: ObligationKey::new(st.role, ObligationKind::Prack, request.cseq().seq()),
            detail: "prack awaiting 200".to_string(),
        },
        now,
    );
    st.obs.record(
        Observation::Subflow { leg: st.role, name: SUBFLOW_EARLY, to: SubflowState::Answered },
        now,
    );
    Ok(())
}
