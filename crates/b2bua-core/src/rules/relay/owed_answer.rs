//! What an offer a responder made on a dialog this stack dialled is owed
//! (RFC 3264 §4): whether the INVITE this stack sent carried the offer —
//! which tells a reliable provisional's description apart as the OFFER or
//! the answer, and so whether its PRACK owes the answer (RFC 3262 §5) — and
//! the ACK that answers an offer the 2xx left to it.

use call::{Call, Leg, OwedAnswer};
use sip_message::SipRequest;

use super::ack::{acked_invite_carries_offer, parse_request};
use super::sdp_session::{continue_on_leg, Author, Carried};
use crate::config::SdpFormPolicy;

/// Whether the INVITE of CSeq `invite_cseq` this stack sent on `leg_id` — the
/// leg's initial INVITE or a re-INVITE on one of its dialogs — carried a
/// session description. `true` where no such INVITE is cached: a provisional
/// whose INVITE this stack cannot read is never taken for an offer.
pub(crate) fn invite_carried_offer(call: &Call, leg_id: &str, invite_cseq: i64) -> bool {
    let Some(leg) = call::helpers::find_leg(call, leg_id) else { return true };
    leg.outbound_invite
        .iter()
        .map(|i| i.txn.as_ref())
        .chain(leg.dialogs.iter().map(|d| d.pending_invite_txn.as_ref()))
        .flatten()
        .filter_map(|h| parse_request(&h.original_invite))
        .find(|r| i64::from(r.cseq().seq()) == invite_cseq)
        .is_none_or(|r| r.sdp().is_some())
}

/// Whether a reliable provisional shown on the responder `tag`'s dialog of
/// `leg_id` carried the offer of the INVITE `invite_cseq` (RFC 3264 §4): that
/// dialog's offer/answer exchange rides the provisional and its PRACK, so the
/// 2xx's description is no offer.
pub(crate) fn offered_reliably(call: &Call, leg_id: &str, tag: &str, invite_cseq: i64) -> bool {
    let shown_with_sdp = call.provisionals.iter().any(|p| {
        p.id.leg == leg_id
            && p.id.responder_tag == tag
            && p.id.invite_cseq.as_i64() == invite_cseq
            && p.shown.is_some()
            && p.responder_sdp
    });
    shown_with_sdp && !invite_carried_offer(call, leg_id, invite_cseq)
}

/// Whether `leg_id`'s confirmed dialog left a provisional's offer to the ACK
/// its 2xx awaits ([`OwedAnswer::DeferredToAck`]).
pub(crate) fn offer_deferred_to_ack(call: &Call, leg_id: &str) -> bool {
    call::helpers::find_leg(call, leg_id).is_some_and(offer_waits_for_ack)
}

fn offer_waits_for_ack(leg: &Leg) -> bool {
    leg.dialogs
        .confirmed()
        .is_some_and(|d| matches!(d.owed_answer, Some(OwedAnswer::DeferredToAck { .. })))
}

/// The answer the caller's `ack` gives, as it leaves on `leg_id`'s confirmed
/// dialog inside the PRACK of the offer deferred to it, its session state
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
        .and_then(|l| l.dialogs.confirmed())
        .map(call::Dialog::id);
    Some(continue_on_leg(
        call,
        leg_id,
        dialog.as_ref(),
        author,
        Carried::InDialog,
        sdp.to_vec(),
        Some(&super::sdp()),
        policy,
    ))
}

/// Whether `leg`'s confirmed dialog holds a 2xx whose ACK this stack composes
/// alone: the §13.2.2.4 obligation is armed and undischarged, and the ACK
/// owes no answer body (RFC 3264 §4) — the INVITE this stack sent carried the
/// offer, or a reliable provisional carried it and the PRACK this stack owes
/// carries the answer (RFC 3262 §5).
pub(crate) fn bare_ack_owed(leg: &Leg) -> bool {
    leg.dialogs.confirmed().is_some_and(|d| {
        d.awaited_ack_cseq.is_some() && (offer_waits_for_ack(leg) || acked_invite_carries_offer(d))
    })
}

/// [`bare_ack_owed`] for `leg_id` of `call`.
pub(crate) fn owes_bare_ack(call: &Call, leg_id: &str) -> bool {
    call::helpers::find_leg(call, leg_id).is_some_and(bare_ack_owed)
}
