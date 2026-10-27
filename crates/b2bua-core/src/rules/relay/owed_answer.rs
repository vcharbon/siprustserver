//! What an offer a responder made on a dialog this stack dialled is owed
//! (RFC 3264 §4): whether the INVITE this stack sent carried the offer —
//! which tells a reliable provisional's description apart as the OFFER or
//! the answer, and so whether its PRACK owes the answer (RFC 3262 §5) — and
//! the ACK that answers an offer the 2xx left to it.

use call::{Call, Leg, OwedAnswer};
use sip_message::{SipRequest, SipResponse};

use super::ack::{acked_invite_carries_offer, parse_request};
use super::sdp_session::{continue_on_leg, Author, Carried};
use crate::config::SdpFormPolicy;

/// Whether the INVITE of CSeq `invite_cseq` this stack sent on `leg_id` — the
/// leg's initial INVITE or a re-INVITE on one of its dialogs — carried a
/// session description. `true` where no such INVITE is cached: a provisional
/// whose INVITE this stack cannot read is never taken for an offer.
pub(crate) fn invite_carried_offer(call: &Call, leg_id: &str, invite_cseq: i64) -> bool {
    let Some(leg) = call::helpers::find_leg(call, leg_id) else { return true };
    leg.dialled_sent()
        .into_iter()
        .chain(leg.dialogs.iter().filter_map(|d| call::helpers::sent_invite_image(leg, d)))
        .filter_map(|sent| parse_request(&sent.image))
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
        call::helpers::awaited_ack_cseq(d).is_some()
            && (offer_waits_for_ack(leg) || acked_invite_carries_offer(leg, d))
    })
}

/// Note on `leg_id`'s confirmed dialog the offer `resp`, the 2xx it took,
/// made where its INVITE delayed it — the INVITE this stack sent carried
/// none, and no reliable provisional of the dialog carried one first (RFC
/// 3262 §5): the ACK numbered `ack_cseq` answers it ([`OwedAnswer::InAck`],
/// RFC 3264 §4).
pub(crate) fn owe_answer_in_ack(call: &mut Call, leg_id: &str, resp: &SipResponse, ack_cseq: i64) {
    let Some(offer) = resp.sdp() else { return };
    let tag = resp.to().tag().unwrap_or_default();
    let invite_cseq = i64::from(resp.cseq().seq());
    if offered_reliably(call, leg_id, tag, invite_cseq) {
        return;
    }
    let offer = offer.to_vec();
    *call = call::helpers::update_leg(call.clone(), leg_id, |leg| {
        let carries = leg.dialogs.confirmed().is_none_or(|d| acked_invite_carries_offer(leg, d));
        if let Some(d) = leg.dialogs.confirmed_mut().filter(|_| !carries) {
            d.owed_answer = Some(OwedAnswer::InAck { ack_cseq, offer });
        }
    });
}

/// The offer `leg`'s confirmed dialog left to the ACK its 2xx still awaits
/// ([`OwedAnswer::InAck`]): the ACK owes its answer (RFC 3264 §4).
pub(crate) fn offer_owed_in_ack(leg: &Leg) -> Option<&[u8]> {
    let d = leg.dialogs.confirmed()?;
    call::helpers::awaited_ack_cseq(d)?;
    match d.owed_answer.as_ref()? {
        OwedAnswer::InAck { offer, .. } => Some(offer),
        _ => None,
    }
}

/// [`bare_ack_owed`] for `leg_id` of `call`.
pub(crate) fn owes_bare_ack(call: &Call, leg_id: &str) -> bool {
    call::helpers::find_leg(call, leg_id).is_some_and(bare_ack_owed)
}

/// Whether the answer `leg_id`'s ACK owes ([`OwedAnswer::InAck`]) can no
/// longer come: the ACK that would carry it acknowledges a 2xx another leg
/// was given, and no leg holds that 2xx still awaiting its ACK (RFC 3264 §4)
/// — its ACK went elsewhere, or no such 2xx was given.
pub(crate) fn answer_never_comes(call: &Call, leg_id: &str) -> bool {
    let Some(leg) = call::helpers::find_leg(call, leg_id) else { return false };
    let Some(cseq) = leg.dialogs.confirmed().and_then(call::helpers::awaited_ack_cseq) else {
        return false;
    };
    let awaited = std::iter::once(&call.a_leg)
        .chain(call.b_legs.iter())
        .filter(|l| l.leg_id != leg_id)
        .flat_map(|l| l.dialogs.iter())
        .filter_map(call::helpers::answered_2xx)
        .any(|u| u.cseq == cseq);
    offer_owed_in_ack(leg).is_some() && !awaited
}
