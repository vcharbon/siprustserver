//! Whether an INVITE this stack sent toward a leg carried the offer — what
//! tells a reliable provisional's description apart as the OFFER (the INVITE
//! carried none, RFC 3264 §4) or the answer, and so whether the PRACK
//! acknowledging it owes the answer (RFC 3262 §5) — and the offer a 2xx left
//! to the caller's ACK, whose answer that PRACK carries.

use call::helpers::OwedPrack;
use call::Call;
use sip_message::SipRequest;

use super::ack::parse_request;
use super::sdp_session::{continue_on_leg, Author, Carried};
use crate::config::SdpFormPolicy;

/// Whether the INVITE of CSeq `invite_cseq` this stack sent on `leg_id` — the
/// leg's initial INVITE or a re-INVITE on one of its dialogs — carried a
/// session description. `true` where no such INVITE is cached: a provisional
/// whose INVITE this stack cannot read is never taken for an offer.
pub(crate) fn invite_carried_offer(call: &Call, leg_id: &str, invite_cseq: i64) -> bool {
    let Some(leg) = call::helpers::find_leg(call, leg_id) else { return true };
    std::iter::once(leg.pending_invite_txn.as_ref())
        .chain(leg.dialogs.iter().map(|d| d.ext.pending_invite_txn.as_ref()))
        .flatten()
        .filter_map(|h| parse_request(&h.original_invite))
        .find(|r| i64::from(r.cseq().seq()) == invite_cseq)
        .is_none_or(|r| r.sdp().is_some())
}

/// The relayed reliable provisionals of `leg_id` whose offer is still owed an
/// answer once its 2xx arrived: the 2xx left the confirmed dialog's offer to
/// the caller's ACK, and PRACKed every other provisional of its INVITE as it
/// arrived. Their PRACK carries the answer her ACK gives (RFC 3262 §5,
/// RFC 3264).
pub(crate) fn offers_owed_at_ack(call: &Call, leg_id: &str) -> Vec<OwedPrack> {
    call::helpers::unacknowledged_relayed_provisionals(call, leg_id, None)
        .into_iter()
        .filter(|o| o.offer.is_some())
        .collect()
}

/// The answer the caller's `ack` gives, as it leaves on `leg_id`'s confirmed
/// dialog inside the PRACK of [`offers_owed_at_ack`], its session state
/// continued there; `None` when the ACK carries no description.
pub(crate) fn ack_answer_on_leg(
    call: &mut Call,
    leg_id: &str,
    ack: &SipRequest,
    author: Author<'_>,
    policy: &dyn SdpFormPolicy,
) -> Option<Vec<u8>> {
    let sdp = ack.sdp()?;
    let dialog = call::helpers::find_leg(call, leg_id)
        .and_then(|l| l.dialogs.first())
        .map(|d| d.sip.remote_tag.clone());
    Some(continue_on_leg(
        call,
        leg_id,
        dialog.as_deref(),
        author,
        Carried::InDialog,
        sdp.to_vec(),
        Some(&super::sdp()),
        policy,
    ))
}
