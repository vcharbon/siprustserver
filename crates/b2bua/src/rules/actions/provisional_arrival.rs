//! The engine's reading of a reliable provisional as it arrives, before any
//! rule (RFC 3262): a copy of one recorded already is the responder's §3
//! retransmission and one out of `RSeq` order is not processed (§4) — both are
//! dropped here, so no rule, CDR or PRACK sees them; any other answering an
//! INVITE still open is recorded, owed its PRACK, with the early dialog it
//! creates (RFC 3261 §12.1.2) and the offer it makes (RFC 3264 §4). A rule
//! only decides whether it is shown.

use call::helpers::Arrival;
use call::{CSeq, Call, OwedAnswer, ProvisionalId, RSeq};
use sip_message::{Method, SipMessage, SipResponse};

use crate::rules::relay;
use b2bua_sdk::event::CallEvent;

use super::select::invite_cseq_from_handle;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Take the reliable provisional `event` carries from `source_leg_id`: a
    /// recorded one is a repeat, a new one out of order is dropped, a new one
    /// in order answering an INVITE still open on that leg is recorded. `None`
    /// for every other event, which the rules read as before.
    pub fn take_reliable_provisional(
        &self,
        call: &mut Call,
        event: &CallEvent,
        source_leg_id: &str,
    ) -> Option<Arrival> {
        let CallEvent::Sip { message, .. } = event else { return None };
        let SipMessage::Response(resp) = message.as_ref() else { return None };
        let id = reliable_provisional(resp, source_leg_id)?;
        let arrival = call::helpers::arrival(call, &id);
        if arrival == Arrival::Repeat {
            return Some(arrival);
        }
        if !invite_open(call, &id) {
            return None;
        }
        if arrival == Arrival::OutOfOrder {
            return Some(arrival);
        }
        let responder_sdp = resp.sdp().is_some();
        let offer = resp.sdp().filter(|_| {
            !call::helpers::described_before(call, &id)
                && !relay::invite_carried_offer(call, source_leg_id, id.invite_cseq.as_i64())
        });
        call::helpers::record_provisional(call, id.clone(), responder_sdp);
        if dials_the_leg(call, &id) {
            self.track_b_early_dialog(call, source_leg_id, resp, &id.responder_tag);
        }
        if let Some(offer) = offer {
            owe_answer_in_prack(call, &id, offer);
        }
        Some(Arrival::InOrder)
    }
}

/// The record of `resp` as a reliable provisional of `leg_id`, when the
/// engine took it.
pub(super) fn recorded(call: &Call, leg_id: &str, resp: &SipResponse) -> Option<ProvisionalId> {
    let id = reliable_provisional(resp, leg_id)?;
    call::helpers::provisional(call, &id).map(|p| p.id.clone())
}

/// The identity of `resp` as a reliable provisional of `leg_id`: `Require:
/// 100rel` with an `RSeq`, a 101–199 status to an INVITE, and a To-tag naming
/// the responder's dialog (RFC 3261 §12.1.2). A CSeq naming no method reads
/// as the INVITE's.
fn reliable_provisional(resp: &SipResponse, leg_id: &str) -> Option<ProvisionalId> {
    let cseq = resp.cseq();
    let answers_invite = cseq.method() == Method::Invite || cseq.method().as_str().is_empty();
    if !(101..200).contains(&resp.status()) || !answers_invite {
        return None;
    }
    let rseq = u32::try_from(relay::reliable_rseq(resp)?).ok()?;
    let tag = resp.to().tag().filter(|t| !t.is_empty())?;
    Some(ProvisionalId {
        leg: leg_id.to_string(),
        responder_tag: tag.to_string(),
        invite_cseq: CSeq(cseq.seq()),
        rseq: RSeq(rseq),
    })
}

/// Whether the INVITE `id` answers still awaits its final on the responder's
/// leg: the INVITE the leg was dialled with before its final, or a re-INVITE
/// in flight on the dialog the responder's tag names — relayed there and
/// awaiting its final, or this stack's own, the round its client-transaction
/// handle holds still open (no 2xx taken). The one reading the screen, the
/// PRACK step and the model invariants share.
pub(crate) fn invite_open(call: &Call, id: &ProvisionalId) -> bool {
    let Some(leg) = call::helpers::find_leg(call, &id.leg) else { return false };
    let cseq = id.invite_cseq.as_i64();
    if let Some(invite) = leg.outbound_invite.as_ref().filter(|i| i.cseq == cseq) {
        return invite.final_received.is_none();
    }
    call::helpers::dialog_with_peer(leg, &id.responder_tag).is_some_and(|d| {
        let relayed = d
            .inbound_pending_requests
            .iter()
            .any(|p| p.method.eq_ignore_ascii_case("INVITE") && p.outbound_cseq == cseq);
        let own_round = invite_cseq_from_handle(d) == Some(cseq)
            && d.ack_branch.is_none()
            && d.awaited_ack_cseq.is_none();
        d.is_confirmed() && (relayed || own_round)
    })
}

/// Whether `id` answers the INVITE this stack dialled its leg with: the one
/// whose provisionals create early dialogs.
fn dials_the_leg(call: &Call, id: &ProvisionalId) -> bool {
    call::helpers::find_leg(call, &id.leg)
        .and_then(|l| l.outbound_invite.as_ref())
        .is_some_and(|i| i.cseq == id.invite_cseq.as_i64())
}

/// The responder's dialog of `id` owes the answer to `offer` in the PRACK of
/// `id` (RFC 3262 §5), unless it already owes one.
fn owe_answer_in_prack(call: &mut Call, id: &ProvisionalId, offer: &[u8]) {
    *call = call::helpers::update_leg(call.clone(), &id.leg, |leg| {
        if let Some(d) = call::helpers::dialog_with_peer_mut(leg, &id.responder_tag) {
            if d.owed_answer.is_none() {
                d.owed_answer = Some(OwedAnswer::InPrack { rseq: id.rseq, offer: offer.to_vec() });
            }
        }
    });
}
