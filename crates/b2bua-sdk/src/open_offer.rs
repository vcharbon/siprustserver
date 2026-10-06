//! Whose INVITE offer is still open on a dialog when a peer sends a request on
//! it (RFC 3264 §4, RFC 3311 §5.1–§5.2): the sender's, so an offer it sends now
//! overlaps its own, or this stack's, so relaying one would put a second offer
//! on a dialog. Only a reliable provisional carrying a description answers an
//! INVITE offer before the final (§5.1); an unreliable one answers nothing.

use call::{Call, Dialog, Leg, LegState, PendingRequest};
use sip_message::header::{HeaderValue, MediaType};
use sip_message::sip_str::SipStr;
use sip_message::{HeaderName, SipMessage, SipParser, SipRequest};

/// The owner of the INVITE offer open on the sender's dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OpenOffer {
    /// The sender's own offer: its INVITE carried one that is unanswered.
    Sender,
    /// An offer of this stack's: shown to the sender in a reliable provisional
    /// and not yet PRACKed, or sent toward a peer that has not answered it.
    Stack,
}

/// The INVITE offer open when `source_leg_id`'s peer sends `req`, if any:
/// its relayed re-INVITE, the call's initial INVITE (the caller) or the INVITE
/// this stack dialled it with (a callee). An offerless INVITE whose offer this
/// stack showed in a reliable provisional is open until the PRACK.
pub fn open_offer(call: &Call, source_leg_id: &str, req: &SipRequest) -> Option<OpenOffer> {
    let shown = req.to().tag().unwrap_or_default();
    let peer = call::helpers::relay_peer_dialog(call, source_leg_id, req.to().tag());
    if let Some(p) = peer.and_then(|(_, d)| pending_invite(d)) {
        return relayed(call, shown, p);
    }
    if source_leg_id == call.a_leg.leg_id {
        return initial(call, shown, peer);
    }
    let leg = call.b_legs.iter().find(|l| l.leg_id == source_leg_id)?;
    dialled(call, leg, req.from().tag().unwrap_or_default())
}

/// The re-INVITE relayed onto `dialog` still awaiting its final.
fn pending_invite(dialog: &Dialog) -> Option<&PendingRequest> {
    dialog.ext.inbound_pending_requests.iter().find(|p| p.method.eq_ignore_ascii_case("INVITE"))
}

/// A relayed re-INVITE: its offer is the sender's until answered, this stack's
/// once the sender CANCELled it; an offerless one opens this stack's offer.
fn relayed(call: &Call, shown: &str, p: &PendingRequest) -> Option<OpenOffer> {
    let cseq = p.inbound_cseq;
    match (p.offered, p.cancelled) {
        (true, _) if shown_answer(call, shown, cseq) => None,
        (true, false) => Some(OpenOffer::Sender),
        (true, true) => Some(OpenOffer::Stack),
        (false, _) => shown_unpracked_offer(call, shown, cseq).then_some(OpenOffer::Stack),
    }
}

/// The caller's initial INVITE while she has no final. Answered on her face
/// but not by the peer dialog the request would be relayed onto (a reroute's
/// new leg), this stack's offer to that peer is the open one.
fn initial(call: &Call, shown: &str, peer: Option<(&Leg, &Dialog)>) -> Option<OpenOffer> {
    if !matches!(call.a_leg.state, LegState::Trying | LegState::Early) {
        return None;
    }
    let invite = &call.a_leg_invite;
    let cseq = i64::from(invite.cseq);
    if !snapshot_carries_sdp(invite) {
        return shown_unpracked_offer(call, shown, cseq).then_some(OpenOffer::Stack);
    }
    if !shown_answer(call, shown, cseq) {
        return Some(OpenOffer::Sender);
    }
    let (leg, dialog) = peer?;
    let pending = matches!(leg.state, LegState::Trying | LegState::Early);
    (pending && !responder_answered(call, &leg.leg_id, &dialog.sip.remote_tag, None))
        .then_some(OpenOffer::Stack)
}

/// A callee before its final: the offer this stack dialled it with stays open
/// on its face until a reliable provisional of its own carries the answer.
fn dialled(call: &Call, leg: &Leg, callee_tag: &str) -> Option<OpenOffer> {
    if !matches!(leg.state, LegState::Trying | LegState::Early) {
        return None;
    }
    let bytes = &leg.pending_invite_txn.as_ref()?.original_invite;
    let SipMessage::Request(invite) =
        sip_message::CustomParser::new().parse(bytes.as_slice()).ok()?
    else {
        return None;
    };
    invite.sdp()?;
    let cseq = i64::from(invite.cseq().seq());
    (!responder_answered(call, &leg.leg_id, callee_tag, Some(cseq))).then_some(OpenOffer::Stack)
}

/// A reliable provisional shown in the `shown` dialog for the INVITE of CSeq
/// `cseq` carried a description: the answer to that INVITE's offer.
fn shown_answer(call: &Call, shown: &str, cseq: i64) -> bool {
    call.reliable_provisionals.iter().any(|r| r.a_tag == shown && r.a_cseq == cseq && r.carried_sdp)
}

/// A reliable provisional shown in the `shown` dialog for the offerless INVITE
/// of CSeq `cseq` carried a description, the offer, and is not yet PRACKed.
fn shown_unpracked_offer(call: &Call, shown: &str, cseq: i64) -> bool {
    call.reliable_provisionals
        .iter()
        .any(|r| r.a_tag == shown && r.a_cseq == cseq && r.carried_sdp && !r.acknowledged)
}

/// The responder on `leg_id`'s dialog `tag` sent a reliable provisional with a
/// description, to the INVITE of CSeq `cseq` where given.
fn responder_answered(call: &Call, leg_id: &str, tag: &str, cseq: Option<i64>) -> bool {
    let of = |c: i64| cseq.is_none_or(|want| want == c);
    call.reliable_provisionals
        .iter()
        .any(|r| r.b_leg_id == leg_id && r.b_tag == tag && of(r.b_cseq) && r.responder_sdp)
        || call.pracked_provisionals.iter().any(|p| {
            p.leg_id == leg_id && p.remote_tag == tag && of(p.invite_cseq) && p.responder_sdp
        })
}

/// The caller's INVITE carried a session description (RFC 5621 §3.1).
fn snapshot_carries_sdp(invite: &call::ALegInviteSnapshot) -> bool {
    invite
        .headers
        .iter()
        .filter(|h| HeaderName::ContentType.matches(&h.name))
        .filter_map(|h| MediaType::parse(&SipStr::owned(&h.value)).ok())
        .any(|ct| sip_message::multipart::sdp_range(&ct, &invite.body).is_some())
}
