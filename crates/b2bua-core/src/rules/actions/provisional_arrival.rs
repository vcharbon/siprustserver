//! The engine's reading of a reliable provisional as it arrives, before any
//! rule (RFC 3262): a copy of one recorded already is the responder's §3
//! retransmission and one out of `RSeq` order is not processed (§4) — both are
//! dropped here, so no rule, CDR or PRACK sees them; any other answering an
//! INVITE still open is recorded, owed its PRACK, with the early dialog it
//! creates (RFC 3261 §12.1.2) and the offer it makes (RFC 3264 §4). A rule
//! only decides whether it is shown. A sibling fork's reaching this stack
//! after a 2xx answered the INVITE is shown to no one: it is PRACKed here at
//! once on its early dialog, which stays or becomes retired as that 2xx
//! retired its siblings', and no rule reads it.

use call::helpers::Arrival;
use call::{CSeq, Call, DialogState, OwedAnswer, ProvisionalId, RSeq};
use sip_message::{Method, SipMessage, SipResponse};
use sip_retransmit::timers::TIMER_M;

use crate::effects::HandlerEffects;
use crate::rules::relay;
use b2bua_sdk::event::CallEvent;

use super::select::invite_cseq_from_handle;
use super::{ActionExecutor, Owed, Settle};

/// What the engine made of a reliable provisional as it arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Taken {
    /// Recorded, owed its PRACK: the rules decide whether it is shown.
    Recorded,
    /// Dropped unread: a repeat, or out of `RSeq` order.
    Dropped(Arrival),
    /// A sibling fork's, past the 2xx that answered the INVITE: PRACKed, its
    /// early dialog retired unless a service tracked it early, read by no
    /// rule.
    PastTheAnswer,
}

impl Taken {
    /// Whether the rules read the turn's event: only a recorded one.
    pub fn read_by_rules(taken: Option<Self>) -> bool {
        matches!(taken, None | Some(Taken::Recorded))
    }
}

impl ActionExecutor<'_> {
    /// Take the reliable provisional `event` carries from `source_leg_id`: a
    /// recorded one is a repeat, a new one out of order is dropped, a new one
    /// in order answering an INVITE still open on that leg is recorded, and a
    /// sibling fork's in order inside 64·T1 of the 2xx that answered the
    /// INVITE dialling the leg is recorded and PRACKed into `fx`
    /// ([`Taken::PastTheAnswer`]). `None` for every other event, which the
    /// rules read as before.
    pub fn take_reliable_provisional(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        event: &CallEvent,
        source_leg_id: &str,
    ) -> Option<Taken> {
        let CallEvent::Sip { message, .. } = event else { return None };
        let SipMessage::Response(resp) = message.as_ref() else { return None };
        let id = reliable_provisional(resp, source_leg_id)?;
        let arrival = call::helpers::arrival(call, &id);
        if arrival == Arrival::Repeat {
            return Some(Taken::Dropped(arrival));
        }
        let sibling = sibling_past_the_answer(call, &id, self.now_ms);
        if sibling.is_none() && !invite_open(call, &id) {
            return None;
        }
        if arrival == Arrival::OutOfOrder {
            return Some(Taken::Dropped(arrival));
        }
        if sibling == Some(Sibling::Retired) {
            set_fork_state(call, &id, DialogState::Retired, DialogState::Early);
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
        let Some(sibling) = sibling else { return Some(Taken::Recorded) };
        let invite = Owed::Invite { leg: source_leg_id, cseq: id.invite_cseq.as_i64() };
        self.settle_owed_pracks(call, fx, invite, Settle::AtFinal { confirmed: None });
        if sibling != Sibling::Tracked {
            set_fork_state(call, &id, DialogState::Early, DialogState::Retired);
        }
        Some(Taken::PastTheAnswer)
    }
}

/// The dialog a sibling fork's reliable provisional past the answer names on
/// its leg: one of those the straggler reading (`fork_straggler`) takes a 2xx
/// of that fork on for a straggler's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Sibling {
    /// None: the fork is heard for the first time.
    Unheard,
    /// One the answer retired, which released no 2xx of its own.
    Retired,
    /// One still early on the confirmed leg (a fork a service tracked late).
    Tracked,
}

/// Which [`Sibling`] `id` is, when it is a sibling fork's reliable
/// provisional reaching this stack after a 2xx answered the INVITE that
/// dialled its leg, inside 64·T1 of that 2xx at `now_ms`, past which the UAC
/// holds the fork's early dialog terminated (RFC 3261 §13.2.2.4). A 2xx the
/// record holds no arrival instant of reads as inside. A retired dialog
/// whose own 2xx was released, or whose release is in flight, is no sibling's.
fn sibling_past_the_answer(call: &Call, id: &ProvisionalId, now_ms: i64) -> Option<Sibling> {
    let leg = call::helpers::find_leg(call, &id.leg)?;
    let dialling = leg.dialled_cseq() == Some(id.invite_cseq.as_i64());
    let invite = leg.outbound_invite().filter(|_| dialling)?;
    let answered = invite.final_received.is_some_and(|status| (200..300).contains(&status));
    let bound = i64::try_from(TIMER_M).unwrap_or(i64::MAX);
    let inside = invite.final_at_ms.is_none_or(|at| now_ms < at.saturating_add(bound));
    if !answered || !inside {
        return None;
    }
    let Some(d) = call::helpers::callee_dialog(leg, &id.responder_tag) else {
        return Some(Sibling::Unheard);
    };
    let released = call::helpers::sent_exchange(d).is_some_and(|x| x.emitted_ack.is_some());
    if d.is_retired() && !released && !d.release_in_flight() {
        Some(Sibling::Retired)
    } else if d.awaits_invite_final() {
        Some(Sibling::Tracked)
    } else {
        None
    }
}

/// Move the early dialog of `id`'s responder from `from` to `to`: reopened
/// so its PRACK may ride it, or retired behind that PRACK as the answer
/// retired its siblings (RFC 3261 §12.1.2) — a 2xx of it is then a
/// straggler, ACKed and BYEd (§13.2.2.4). Nothing for a dialog in another
/// state.
fn set_fork_state(call: &mut Call, id: &ProvisionalId, from: DialogState, to: DialogState) {
    call::helpers::update_leg(call, &id.leg, |leg| {
        if let Some(d) = call::helpers::dialog_with_peer_mut(leg, &id.responder_tag) {
            if d.state == from {
                d.state = to;
            }
        }
    });
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
    let tag = resp.to().tag()?;
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
/// awaiting its final, or this stack's own, the INVITE the dialog's record
/// holds with no 2xx taken. The one reading the screen, the PRACK step and the
/// model invariants share.
pub fn invite_open(call: &Call, id: &ProvisionalId) -> bool {
    let Some(leg) = call::helpers::find_leg(call, &id.leg) else { return false };
    let cseq = id.invite_cseq.as_i64();
    if let Some(invite) = leg.outbound_invite().filter(|_| leg.dialled_cseq() == Some(cseq)) {
        return invite.final_received.is_none();
    }
    call::helpers::dialog_with_peer(leg, &id.responder_tag).is_some_and(|d| {
        let relayed = call::helpers::invite_relay(d).is_some_and(|p| p.outbound_cseq == cseq);
        let own_round =
            invite_cseq_from_handle(leg, d) == Some(cseq) && !call::helpers::invite_taken(d);
        d.is_confirmed() && (relayed || own_round)
    })
}

/// Whether `id` answers the INVITE this stack dialled its leg with: the one
/// whose provisionals create early dialogs.
fn dials_the_leg(call: &Call, id: &ProvisionalId) -> bool {
    call::helpers::find_leg(call, &id.leg)
        .and_then(|l| l.dialled_cseq())
        .is_some_and(|cseq| cseq == id.invite_cseq.as_i64())
}

/// The responder's dialog of `id` owes the answer to `offer` in the PRACK of
/// `id` (RFC 3262 §5), unless it already owes one.
fn owe_answer_in_prack(call: &mut Call, id: &ProvisionalId, offer: &[u8]) {
    call::helpers::update_leg(call, &id.leg, |leg| {
        if let Some(d) = call::helpers::dialog_with_peer_mut(leg, &id.responder_tag) {
            if d.owed_answer.is_none() {
                d.owed_answer = Some(OwedAnswer::InPrack { rseq: id.rseq, offer: offer.to_vec() });
            }
        }
    });
}
