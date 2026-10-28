//! The PRACKs this stack owes the responders of reliable provisionals (RFC
//! 3262 §4): one step settles every recorded provisional whose responder is
//! still owed one, called by the engine after the rules and inside each
//! primitive that ends a dialog or an INVITE — the CANCEL and BYE builders,
//! the fork retirement of a 2xx, the final that rejects an INVITE — so the
//! PRACK leaves ahead of what ends its dialog. No rule PRACKs.

use call::helpers::bump_our_cseq;
use call::TxnKind;
use call::{
    Call, DialogState, EndingBy, LegDisposition, OwedAnswer, Provisional, ProvisionalId,
    ResponderAck,
};
use sip_message::generators::{self, GenerateInDialogRequestOpts, InDialogMethod};
use sip_message::header::RAck;
use sip_message::Method;

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::relay;

use super::select::leg_at;
use super::ActionExecutor;

/// Which owed PRACKs a settlement covers.
#[derive(Clone, Copy, Debug)]
pub enum Owed<'a> {
    /// Every provisional `leg` raised: its dialogs are ending.
    Leg(&'a str),
    /// The provisionals answering `leg`'s INVITE of CSeq `cseq`: that INVITE
    /// took, or is about to take, its final.
    Invite { leg: &'a str, cseq: i64 },
    /// Every provisional of the call, read at the end of the turn.
    Call,
}

/// Why a settlement runs, which fixes what it PRACKs and with what answer.
#[derive(Clone, Copy, Debug)]
pub enum Settle<'a> {
    /// The INVITE's final: a 2xx confirming the responder dialog `confirmed`
    /// leaves the offer that dialog's shown provisional carried to the ACK
    /// it awaits; every other owed PRACK leaves now.
    AtFinal { confirmed: Option<&'a str> },
    /// The dialog ends: every owed PRACK leaves now, the one waiting for the
    /// ACK included, an owed offer answered rejecting every stream.
    Ending,
    /// The turn's end: a provisional shown to no one, or one whose INVITE,
    /// leg or dialog is over, is PRACKed; one the party it was shown to may
    /// still PRACK waits for that PRACK.
    TurnEnd,
}

impl ActionExecutor<'_> {
    /// Settle the owed PRACKs `scope` covers, as `why` says: each leaves on
    /// its own early dialog in recorded order — `RSeq` order within a dialog
    /// (RFC 3262 §4) — or, where that dialog no longer carries one, is waived
    /// (RFC 3261 §15.1.2). A PRACK leaves at most once per provisional.
    pub fn settle_owed_pracks(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        scope: Owed<'_>,
        why: Settle<'_>,
    ) {
        let owed: Vec<ProvisionalId> = call
            .provisionals
            .iter()
            .filter(|p| p.responder.is_owed() && in_scope(p, scope))
            .filter(|p| due(call, p, why))
            .map(|p| p.id.clone())
            .collect();
        for id in owed {
            if let Settle::AtFinal { confirmed: Some(tag) } = why {
                if defer_to_ack(call, &id, tag) {
                    continue;
                }
            }
            let Some(p) = call::helpers::provisional(call, &id).cloned() else { continue };
            if responder_dialog(call, &p) == Some(false) {
                call::helpers::note_responder_ack(call, &id, ResponderAck::Waived);
                clear_owed_offer(call, &id);
                continue;
            }
            let answer = owed_offer(call, &id).and_then(|offer| self.rejecting_answer(&offer));
            self.send_prack_to_leg(call, fx, &id, answer);
        }
    }

    /// PRACK the provisional whose offer the 2xx on `leg_id`'s confirmed
    /// dialog left to the ACK, as that ACK leaves: the PRACK carries
    /// `answer`, or rejects every stream where the ACK gives none (RFC 3262
    /// §5, RFC 3264 §6). Nothing when no offer waits for the ACK.
    pub(super) fn prack_deferred_offer(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        answer: Option<Vec<u8>>,
    ) {
        let Some((tag, rseq, offer)) = deferred_offer(call, leg_id) else { return };
        let id = call.provisionals.iter().find(|p| {
            p.id.leg == leg_id
                && p.id.responder_tag == tag
                && p.id.rseq == rseq
                && p.responder == ResponderAck::DeferredToAck
        });
        let Some(id) = id.map(|p| p.id.clone()) else { return };
        let body = answer.or_else(|| self.rejecting_answer(&offer));
        self.send_prack_to_leg(call, fx, &id, body);
    }

    /// Originate the PRACK of the provisional `id` on its responder's dialog,
    /// selected strictly by the responder's tag (forks are independent
    /// dialogs), carrying `answer` where it answers an offer the provisional
    /// carried (RFC 3262 §5). The `RAck` is `<rseq> <invite-cseq> INVITE`
    /// (§7.2); the dialog's local CSeq advances by one. The responder's PRACK
    /// is then this stack's own, on the recorded client branch, and the offer
    /// its answer rode is no longer owed.
    fn send_prack_to_leg(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        id: &ProvisionalId,
        answer: Option<Vec<u8>>,
    ) {
        let Some(leg) = leg_at(call, &id.leg) else {
            self.turn.notes.defect(
                crate::turn_notes::PRACK_HAS_A_LEG,
                format!("an owed PRACK names leg {}, which the call does not hold", id.leg),
                call,
            );
            return;
        };
        let Some(dialog) = call::helpers::dialog_with_peer(leg, &id.responder_tag).cloned() else {
            self.turn.notes.defect(
                crate::turn_notes::PRACK_HAS_A_DIALOG,
                format!(
                    "an owed PRACK names dialog {} on leg {}, which the leg does not hold",
                    id.responder_tag, id.leg
                ),
                call,
            );
            return;
        };
        let t_id = dialog.id();
        // Per-dialog CSeq (RFC 3261 §12.2.1.1): each forked early dialog has
        // its own sequence.
        let outbound_cseq = dialog.our_cseq + 1;
        let gen_dialog = relay::to_gen_dialog(leg, &dialog);
        let branch = self.turn.ids.new_branch();
        let marks = self.marks(call);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(
                self.config,
                marks,
                &id.leg,
                Some(relay::RequestFacts {
                    method: "PRACK",
                    uri: &gen_dialog.remote_target,
                    offer: false,
                }),
                branch.clone(),
            )),
            contact: relay::leg_contact(call, &id.leg),
            rack: Some(RAck::new(id.rseq.0, id.invite_cseq.0, Method::Invite)),
            cseq: Some(relay::wire_cseq(outbound_cseq)),
            content_type: answer.is_some().then(relay::sdp),
            body: answer.unwrap_or_default(),
            ..Default::default()
        };
        // The call records the PRACK only once it is built.
        let res =
            match generators::generate_in_dialog_request(InDialogMethod::Prack, &gen_dialog, &opts)
            {
                Ok(res) => res,
                Err(e) => {
                    tracing::warn!(call_ref = %call.call_ref, leg_id = %id.leg, error = %e,
                    "PRACK not sent: the dialog's state makes no request");
                    return;
                }
            };
        bump_our_cseq(call, &id.leg, &t_id, 1);
        call::helpers::note_responder_ack(call, id, ResponderAck::OwnPrack);
        clear_owed_offer(call, id);
        let dest = relay::target_dest(&gen_dialog.remote_target);
        let (out_req, dest) = relay::apply_b_leg_egress(
            self.config,
            &id.leg,
            &gen_dialog.route_set,
            res.request,
            dest,
        );
        let prack = OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("PRACK → {}", id.leg),
            leg_id: Some(id.leg.clone()),
            provenance: Provenance::Authored,
        };
        self.record_own_effect(call, &id.leg, &t_id, &prack);
        fx.outbound.push(prack);
    }
}

/// Whether `p` is one `scope` covers.
fn in_scope(p: &Provisional, scope: Owed<'_>) -> bool {
    match scope {
        Owed::Leg(leg) => p.id.leg == leg,
        Owed::Invite { leg, cseq } => p.id.leg == leg && p.id.invite_cseq.as_i64() == cseq,
        Owed::Call => true,
    }
}

/// Whether the owed PRACK of `p` leaves under `why`. One waiting for the ACK
/// leaves only as its dialog ends.
fn due(call: &Call, p: &Provisional, why: Settle<'_>) -> bool {
    if p.responder == ResponderAck::DeferredToAck {
        return matches!(why, Settle::Ending) || responder_dialog(call, p) != Some(true);
    }
    match why {
        Settle::AtFinal { .. } | Settle::Ending => true,
        Settle::TurnEnd => {
            let leg_over = call::helpers::find_leg(call, &p.id.leg).is_none_or(|l| {
                matches!(l.disposition(), LegDisposition::Cancelling)
                    || l.state() == call::LegState::Terminated
            });
            p.shown.is_none() || leg_over || !super::invite_open(call, &p.id)
        }
    }
}

/// Whether the responder's dialog of `p` still carries a PRACK: `Some(true)`
/// early, confirmed, or an early dialog whose INVITE this side is cancelling
/// — a CANCEL does not end the INVITE transaction (RFC 3262 §4);
/// `Some(false)` retired, ended, or ending by this side's BYE — nothing rides
/// it then (RFC 3261 §15.1.2); `None` when the leg holds no such dialog.
fn responder_dialog(call: &Call, p: &Provisional) -> Option<bool> {
    let d = call::helpers::find_leg(call, &p.id.leg)
        .and_then(|l| call::helpers::dialog_with_peer(l, &p.id.responder_tag))?;
    Some(d.is_live() && d.state != DialogState::Ending(EndingBy::Bye))
}

/// Leave the offer the shown provisional `id` carried to the ACK the 2xx
/// confirming its responder dialog `confirmed` awaits (RFC 3262 §5, RFC 3264):
/// the dialog's caller answers in that ACK. Returns whether it was left.
fn defer_to_ack(call: &mut Call, id: &ProvisionalId, confirmed: &str) -> bool {
    if id.responder_tag != confirmed
        || call::helpers::provisional(call, id).is_none_or(|p| p.shown.is_none())
    {
        return false;
    }
    let mut deferred = false;
    call::helpers::update_leg(call, &id.leg, |leg| {
        let Some(d) = call::helpers::dialog_with_peer_mut(leg, &id.responder_tag) else {
            return;
        };
        if let Some(OwedAnswer::InPrack { rseq, offer }) = d.owed_answer.clone() {
            if rseq == id.rseq {
                d.owed_answer = Some(OwedAnswer::DeferredToAck { rseq, offer });
                deferred = true;
            }
        }
    });
    if deferred {
        call::helpers::note_responder_ack(call, id, ResponderAck::DeferredToAck);
    }
    deferred
}

/// The offer the PRACK of `id` answers, as its responder dialog owes it.
fn owed_offer(call: &Call, id: &ProvisionalId) -> Option<Vec<u8>> {
    let leg = call::helpers::find_leg(call, &id.leg)?;
    call::helpers::dialog_with_peer(leg, &id.responder_tag)?
        .owed_answer
        .as_ref()?
        .prack_offer(id.rseq)
        .map(<[u8]>::to_vec)
}

/// The offer `leg_id`'s confirmed dialog left to its ACK: the responder tag,
/// the provisional that carried it, and the offer.
fn deferred_offer(call: &Call, leg_id: &str) -> Option<(String, call::RSeq, Vec<u8>)> {
    let d = call::helpers::find_leg(call, leg_id)?.dialogs.confirmed()?;
    match d.owed_answer.as_ref()? {
        OwedAnswer::DeferredToAck { rseq, offer } => {
            Some((d.peer_tag.clone(), *rseq, offer.clone()))
        }
        _ => None,
    }
}

/// The offer the provisional `id` carried is no longer owed by its dialog: a
/// PRACK naming it has left, or its dialog is gone.
pub(super) fn clear_owed_offer(call: &mut Call, id: &ProvisionalId) {
    call::helpers::update_leg(call, &id.leg, |leg| {
        if let Some(d) = call::helpers::dialog_with_peer_mut(leg, &id.responder_tag) {
            if d.owed_answer.as_ref().is_some_and(|o| o.prack_offer(id.rseq).is_some()) {
                d.owed_answer = None;
            }
        }
    });
}
