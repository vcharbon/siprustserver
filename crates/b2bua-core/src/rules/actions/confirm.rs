//! The confirmation a 2xx to a callee leg's INVITE makes (RFC 3261 §12.1.2,
//! §13.2.2.4): one chain of steps whose order the types carry. The winner is
//! promoted ([`Promoted`]), the PRACKs the INVITE's responders are owed leave,
//! the losers retire ([`Retired`]), and only a retired fork set lets the
//! caller's dialog be re-identified and the ACK obligation be armed.

use call::helpers::{callee_dialog_mut, fronting_dialog, is_adopted};
use call::{
    Call, CallModelState, DialogId, DialogState, LegDisposition, OwedAnswer, RetiredDialog,
};
use sip_message::header;
use sip_message::SipResponse;

use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

use super::dialog_track::{adopt_a_tag, contact_uri, own_a_dialog_mut, uac_route_set};
use crate::effects::HandlerEffects;

use super::{ActionExecutor, Owed, Settle};

/// The callee dialog the 2xx carries is confirmed; the leg's other early
/// dialogs, `losers`, are still early.
struct Promoted {
    leg_id: String,
    /// The callee tag the 2xx carries; empty for a tagless 2xx, which names
    /// no dialog.
    winner: String,
    losers: Vec<DialogId>,
    /// The 2xx crossed this leg's CANCEL: it is reaped, never relayed.
    crossed_cancel: bool,
}

/// The callee leg holds one live dialog, the winner's: every loser retired.
struct Retired {
    leg_id: String,
    winner: String,
    crossed_cancel: bool,
}

impl ActionExecutor<'_> {
    /// Confirm the callee dialog the turn's 2xx answers on `leg_id`, PRACK
    /// what the INVITE's responders are still owed (RFC 3262 §4) — the
    /// winner's offer left to the ACK — on each fork before the losers
    /// retire, give the caller the dialog that 2xx is relayed on, and arm the
    /// ACK the 2xx owes. Nothing happens without a response event.
    pub(super) fn confirm(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
    ) {
        let Some(resp) = ctx.response() else { return };
        let promoted = self.promote_winner(call, resp, leg_id);
        self.settle_owed_pracks(
            call,
            fx,
            Owed::Invite { leg: leg_id, cseq: i64::from(resp.cseq().seq()) },
            Settle::AtFinal { confirmed: Some(&promoted.winner) },
        );
        let retired = retire_losers(call, promoted);
        self.reidentify_caller_dialog(call, resp, &retired);
        arm_ack_obligation(call, resp, retired);
    }

    /// Confirm the dialog whose callee tag the 2xx carries — minted now for a
    /// tag no provisional rang (§12.1.2), with the INVITE-seeded sequence —
    /// learning its target and route set (§12.1.2: the 2xx's Record-Route,
    /// reversed per recorded route). Its session becomes the leg's, and the
    /// 2xx the final of the leg's INVITE. `peer_cseq` stays as the callee's early
    /// requests left it: the 2xx echoes a number of our own sequence.
    fn promote_winner(&mut self, call: &mut Call, resp: &SipResponse, leg_id: &str) -> Promoted {
        let winner = resp.to().tag().unwrap_or_default().to_string();
        let crossed_cancel = call
            .b_legs
            .iter()
            .any(|l| l.leg_id == leg_id && l.disposition() == LegDisposition::Cancelling);
        let remote_target = contact_uri(resp.header::<header::Contact>(), &call.call_ref, leg_id)
            .unwrap_or_default();
        let route_set = self.dialog_route_set(uac_route_set(resp), &call.call_ref, leg_id);
        if !winner.is_empty() {
            self.track_b_early_dialog(call, leg_id, resp, &winner);
        }
        let mut losers = Vec::new();
        if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
            if let Some(d) = callee_dialog_mut(leg, &winner) {
                if !remote_target.is_empty() {
                    d.remote_target = remote_target;
                }
                if !route_set.is_empty() {
                    d.route_set = route_set;
                }
                // A fork its leg's end retired answers anyway: the relays
                // it held open are the confirmed dialog's again.
                if let DialogState::Retired(r) = &mut d.state {
                    d.requests.append(&mut r.open_requests);
                }
                d.state = DialogState::Confirmed;
                relay::adopt_confirmed_dialog(leg, &winner);
            }
            losers =
                leg.dialogs.iter().filter(|d| d.awaits_invite_final()).map(|d| d.id()).collect();
            if let Some(invite) = leg.outbound_invite.as_mut() {
                invite.record_final(resp.status());
            }
        }
        Promoted { leg_id: leg_id.to_string(), winner, losers, crossed_cancel }
    }

    /// Give the caller the dialog the 2xx is relayed on (§12.1.1): when the
    /// 2xx is the answer to her INVITE, the a-leg is confirmed and the
    /// caller-facing dialog fronting the winner (or the one the relay will
    /// show it under) is confirmed, the others retiring. The answer body
    /// relayed toward her is kept as her dialog's cached SDP.
    fn reidentify_caller_dialog(&mut self, call: &mut Call, resp: &SipResponse, retired: &Retired) {
        let Retired { leg_id, winner, crossed_cancel } = retired;
        // The answer to her INVITE: relayed — a live call (a terminating
        // call's caller is owed her unanswered final, ADR-0022), an adopted
        // leg (a media server behind early media is the service's, ADR-0016),
        // not a 2xx crossing the leg's CANCEL, which is reaped — and nothing
        // has answered her yet. The tag is read as an answer's
        // (`a_face_tag`): the relay that follows decides the a-face alike.
        let adopted = call.b_legs.iter().find(|l| &l.leg_id == leg_id).is_none_or(is_adopted);
        let answers_the_caller = call.state == CallModelState::Active
            && adopted
            && !crossed_cancel
            && call.a_leg.invite_final_sent.is_none();
        // The caller-facing tag of the dialog this 2xx creates: the one its
        // relayed provisional or `MapUnshownDialog` showed, or the one the
        // relay of a live answer takes now (`a_face_tag`). A 2xx crossing the
        // leg's CANCEL that no caller-facing dialog fronts gives her none.
        let preferred = match fronting_dialog(call, leg_id, winner) {
            Some(d) => Some(d.our_tag.clone()),
            None if answers_the_caller && !winner.is_empty() => {
                Some(self.a_face_tag(call, leg_id, winner, true))
            }
            None => None,
        };
        if preferred.is_some() || !crossed_cancel {
            self.ensure_a_dialog_with(call, preferred.clone());
        }
        if let Some(pref) = preferred.filter(|p| !p.is_empty() && answers_the_caller) {
            adopt_a_tag(call, &pref);
        }
        // The relayFirst18x / fake-PRACK cache: the body the relay carries
        // toward the caller, the policy override else the callee's.
        let answer_body = match call.policy_update_body.clone() {
            Some(call::PolicyUpdateBody::Bytes(b)) => Some(b),
            _ if !resp.body().is_empty() => Some(resp.body().to_vec()),
            _ => None,
        };
        if let (Some(body), Some(d)) = (answer_body, own_a_dialog_mut(call)) {
            d.cached_sdp = Some(body);
        }
    }
}

/// Retire every loser (§12.1.2): with its sequence, which the BYE releasing
/// a late 2xx of it continues (§12.2.1.1), and the relays still open in it,
/// whose finals still reach their originators (§8.1.3.3).
fn retire_losers(call: &mut Call, promoted: Promoted) -> Retired {
    let Promoted { leg_id, winner, losers, crossed_cancel } = promoted;
    if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
        for id in &losers {
            if let Some(lost) = leg.dialogs.by_tags_mut(&id.our_tag, &id.peer_tag) {
                let open_requests = std::mem::take(&mut lost.requests);
                lost.state = DialogState::Retired(RetiredDialog { open_requests, release: None });
            }
        }
    }
    Retired { leg_id, winner, crossed_cancel }
}

/// Arm the ACK the 2xx owes (§13.2.2.4): the caller's own, relayed, carrying
/// her INVITE's CSeq. The ACK's emission mints its client transaction and the
/// branch every repeat of the 2xx is re-ACKed on. A delayed offer — the
/// INVITE this stack sent carried none, and no reliable provisional of the
/// dialog carried one first (RFC 3262 §5) — makes the 2xx's description the
/// offer that ACK answers ([`OwedAnswer::InAck`]).
fn arm_ack_obligation(call: &mut Call, resp: &SipResponse, retired: Retired) {
    let Retired { leg_id, winner, .. } = retired;
    let awaited_ack_cseq = i64::from(call.a_leg_invite.cseq);
    let invite_cseq = i64::from(resp.cseq().seq());
    let offered_reliably = relay::offered_reliably(call, &leg_id, &winner, invite_cseq);
    let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) else { return };
    let Some(d) = leg.dialogs.confirmed_mut() else { return };
    d.awaited_ack_cseq = Some(awaited_ack_cseq);
    let delayed = !offered_reliably && !relay::acked_invite_carries_offer(d);
    if let Some(offer) = resp.sdp().filter(|_| delayed) {
        d.owed_answer =
            Some(OwedAnswer::InAck { ack_cseq: awaited_ack_cseq, offer: offer.to_vec() });
    }
}
