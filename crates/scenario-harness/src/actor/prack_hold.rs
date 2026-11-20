//! A PRACK whose `200` a held callee answers late: the provisional it names
//! asked for a hold ([`super::Provisional::prack_answer_after`]), so the PRACK
//! transaction stays open across later provisionals, other dialogs' PRACKs
//! and the final (RFC 3262 §3 has the PRACK acknowledge on arrival; its `200`
//! closes only its own non-INVITE transaction).

use sip_message::header::RAck;
use tokio::time::Instant;

use super::ledger::{ObligationKey, ObligationKind};
use super::runner::ActorState;
use super::state::Observation;
use crate::{ServerTxn, StepError};

/// Answer the PRACK `uas` now, or hold its `200` for the delay its
/// provisional asked for. A held `200` is an obligation owed whatever
/// becomes of the dialog: the call settles only once it is sent.
pub(super) async fn answer(st: &mut ActorState<'_>, mut uas: ServerTxn) -> Result<(), StepError> {
    let tag = uas.request().to().tag().unwrap_or_default().to_string();
    let rseq = uas.request().header::<RAck>().and_then(Result::ok).map(|r| r.rseq());
    let hold = rseq.and_then(|rseq| st.trains.prack_holds.remove(&(tag, rseq)));
    let Some(after) = hold.filter(|d| !d.is_zero()) else {
        return uas.respond(200, "OK").try_send().await;
    };
    st.trains.holds_opened += 1;
    let key = ObligationKey::new(st.role, ObligationKind::HeldPrack200, st.trains.holds_opened);
    let now = Instant::now();
    st.obs.record(Observation::RequestSent { key, detail: "PRACK 200 held".to_string() }, now);
    st.trains.held_200s.push((now + after, key, Box::new(uas)));
    Ok(())
}

/// When the earliest held `200` is due.
pub(super) fn due(st: &ActorState<'_>) -> Option<Instant> {
    st.trains.held_200s.iter().map(|(at, ..)| *at).min()
}

/// Send every held `200` now due.
pub(super) async fn fire(st: &mut ActorState<'_>) -> Result<(), StepError> {
    let now = Instant::now();
    let (due, kept): (Vec<_>, Vec<_>) =
        std::mem::take(&mut st.trains.held_200s).into_iter().partition(|(at, ..)| *at <= now);
    st.trains.held_200s = kept;
    for (_, key, mut uas) in due {
        uas.respond(200, "OK").try_send().await?;
        st.obs.record(Observation::ResponseObserved { key }, now);
    }
    Ok(())
}
