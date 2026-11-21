//! ACK origination toward a leg (RFC 3261 §13.2.2.4): the ACK of a 2xx this
//! stack's INVITE drew, relayed from the peer or composed on its own account,
//! retained for the 2xx's retransmissions, and the answer a delayed offer is
//! owed in it (RFC 3264 §4). A retained ACK is sent again only for the copy
//! of its final the count marked owed (ADR-0057 §10, [`re_ack_copy`]),
//! whichever rule asks; the ACK carried ahead of a dialog's last INVITE, only
//! for such a copy of the 2xx before it ([`re_ack_ahead`]).

use call::{Call, InviteExchange, RetainedEmission};
use sip_message::header::MediaType;
use sip_message::SipHeader;

use crate::effects::{HandlerEffects, OutboundBody, Provenance, QuietTurn};
use crate::rules::relay;
use b2bua_sdk::model::{Body, RuleContext};

use super::ActionExecutor;

/// The ACK `x`, an INVITE's exchange, retains, re-passed to the transport as it
/// left (§13.2.2.4: answer included on a delayed offer) under `label`, toward
/// `leg_id`, when the copy of its final the turn counted is owed it
/// ([`relay::re_ack_owed`]); nothing otherwise: a copy past the answerer's
/// ladder, or no copy at all.
pub(super) fn re_ack_copy(
    fx: &mut HandlerEffects,
    x: &InviteExchange,
    label: String,
    leg_id: &str,
) {
    let Some(retained) = x.emitted_ack.as_ref().filter(|_| relay::re_ack_owed(x)) else {
        return;
    };
    fx.outbound.push(super::ladder::repeat_of(retained, label, leg_id));
    fx.quiet = Some(QuietTurn::ReAck);
}

/// The ACK `leg_id`'s dialog named by `ctx` carries ahead of its last INVITE
/// ([`call::AckAhead`]), re-passed to the transport as it left, when `ctx`'s
/// event is a copy of the 2xx that ACK acknowledges ([`relay::copy_of_2xx_ahead`])
/// the count marked owed it ([`relay::re_ack_ahead_owed`]); nothing otherwise.
pub(super) fn re_ack_ahead(call: &Call, fx: &mut HandlerEffects, ctx: &RuleContext, leg_id: &str) {
    let (Some(resp), Some(id)) = (ctx.response(), ctx.resolved.dialog.as_ref()) else { return };
    let x = call::helpers::find_leg(call, leg_id)
        .and_then(|leg| leg.dialogs.by_tags(&id.our_tag, &id.peer_tag))
        .filter(|d| relay::copy_of_earlier_2xx(d, resp))
        .and_then(call::helpers::sent_exchange)
        .filter(|x| relay::copy_of_2xx_ahead(x, resp));
    let Some(ahead) = x.and_then(|x| x.ack_ahead.as_ref()).filter(|a| relay::re_ack_ahead_owed(a))
    else {
        return;
    };
    let label = format!("ACK (re-ACK, earlier INVITE's 2xx) → {leg_id}");
    fx.outbound.push(super::ladder::repeat_of(&ahead.ack, label, leg_id));
    fx.quiet = Some(QuietTurn::ReAck);
}

impl ActionExecutor<'_> {
    /// ACK `leg_id`'s confirmed dialog, carrying `body`/`content_type` through
    /// (a delayed-offer answer rides the ACK, RFC 3261 §13.2.2.4 / RFC 3264 §4)
    /// and `extra_headers`, the relayed ACK's end-to-end lines. `provenance`
    /// says whose ACK it is: the peer's, relayed, or one this stack composes on
    /// its own account; `author` whose description `body` is.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ack_leg(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        body: Vec<u8>,
        content_type: Option<MediaType>,
        extra_headers: Vec<SipHeader>,
        provenance: Provenance,
        author: relay::Author<'_>,
    ) {
        let parts = (body, content_type, extra_headers);
        self.ack_leg_at(call, fx, leg_id, None, parts, provenance, author);
    }

    /// [`Self::ack_leg`] for the 2xx to the INVITE numbered `acked`, when the
    /// record holds no image of it: the ACK takes that number (RFC 3261
    /// §13.2.2.4). `None` reads the number off the dialog's INVITE record.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn ack_leg_at(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        acked: Option<u32>,
        (body, content_type, extra_headers): (Vec<u8>, Option<MediaType>, Vec<SipHeader>),
        provenance: Provenance,
        author: relay::Author<'_>,
    ) {
        // The ACK rides the leg's dialog: the one a 2xx established.
        let established =
            call::helpers::find_leg(call, leg_id).and_then(|l| l.dialogs.established());
        if body.is_empty() {
            let retained = established
                .and_then(call::helpers::sent_exchange)
                .filter(|x| x.emitted_ack.is_some());
            if let Some(x) = retained {
                let label = format!("ACK (re-ACK, final retransmit) → {leg_id}");
                return re_ack_copy(fx, x, label, leg_id);
            }
        }
        let dialog = established.map(call::Dialog::id);
        let description = relay::description_in(&body, content_type.as_ref(), &extra_headers);
        let body = relay::continue_on_leg(
            call,
            leg_id,
            dialog.as_ref(),
            author,
            relay::Carried::InDialog,
            body,
            description,
            self.config.sdp_form.as_ref(),
        );
        let leg = if leg_id == call.a_leg.leg_id {
            Some(&call.a_leg)
        } else {
            call.b_legs.iter().find(|l| l.leg_id == leg_id)
        };
        let ack = leg.and_then(|leg| {
            relay::ack_b_leg(
                self.marks(call),
                leg,
                self.config,
                &mut self.turn.ids,
                acked,
                body,
                content_type,
                extra_headers,
                provenance,
            )
        });
        if let Some((e, branch)) = ack {
            let OutboundBody::Request(r) = &e.body else { return };
            let cseq = i64::from(r.cseq().seq());
            // Retain THE ACK's exact bytes + destination before it leaves, so a
            // retransmitted 2xx re-passes this datagram (§13.2.2.4); the
            // end-of-turn stamp rebinds it to the bytes that leave.
            let retained = RetainedEmission::on_trigger(
                r.image().to_vec(),
                e.destination.clone(),
                call::Repeated::request(r.method().as_str()),
            );
            call::helpers::retain_emitted_ack(call, leg_id, retained, cseq);
            fx.outbound.push(e);
            // Retain the ACK branch so a retransmitted 2xx is re-ACKed on the
            // SAME transaction (§13.2.2.4 — `re-ack-retransmitted-2xx`).
            call::helpers::retain_ack_branch(call, leg_id, &branch, cseq);
            // The ACK this dialog owed is on the wire, so the §13.2.2.4
            // obligation is discharged HERE — by the emission, whoever composed
            // it. A peer ACK for the same 2xx is then absorbed rather than
            // relayed on top of it (`relay_request`'s `owed` gate). A dialog
            // whose ACK nobody composes keeps the obligation and gets the peer's.
            call::helpers::set_awaited_ack_cseq(call, leg_id, None);
            // A delayed offer's answer rode this ACK (RFC 3264 §4).
            call::helpers::update_leg(call, leg_id, |l| {
                if let Some(d) = l.dialogs.established_mut() {
                    if matches!(d.owed_answer, Some(call::OwedAnswer::InAck { .. })) {
                        d.owed_answer = None;
                    }
                }
            });
        }
    }

    /// ACK `leg_id`'s confirmed dialog as a rule authors it
    /// ([`b2bua_sdk::model::RuleAction::AckLeg`]): a body-bearing ACK carries a
    /// delayed-offer answer (RFC 3261 §13.2.2.4), its type `application/sdp`
    /// when none is given; an empty ACK stays a bare ACK — no body, no
    /// Content-Type — unless the 2xx made an offer the ACK must answer, which
    /// it then answers rejecting every stream (RFC 3264 §6).
    pub(super) fn ack_authored(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        body: &Option<Body>,
    ) {
        let (bytes, content_type, author, descriptors) = super::dispatch::parts(body);
        if bytes.is_empty() {
            let owed = call::helpers::find_leg(call, leg_id).and_then(relay::offer_owed_in_ack);
            if owed.is_some() {
                self.ack_rejecting(call, fx, leg_id);
            } else {
                self.ack_own_bare(call, fx, leg_id);
            }
            return;
        }
        let ct = content_type.and_then(relay::media_type).unwrap_or_else(relay::sdp);
        let mut own = Vec::new();
        relay::describe_body(&mut own, bytes, descriptors);
        let body = bytes.to_vec();
        self.ack_leg(call, fx, leg_id, body, Some(ct), own, Provenance::Authored, author);
    }

    /// ACK `leg_id`'s confirmed dialog with `answer`, another leg's answer
    /// ([`b2bua_sdk::model::RuleAction::AckLegAnswering`]): carried where the
    /// dialog's 2xx made an offer its ACK owes the answer to, awaited where it
    /// did and none is given yet, left out where it made none.
    pub(super) fn ack_answering(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        answer: &Option<Body>,
    ) {
        let owed = call::helpers::find_leg(call, leg_id).and_then(relay::offer_owed_in_ack);
        match (owed.is_some(), answer) {
            (true, None) => {}
            (true, Some(_)) => self.ack_authored(call, fx, leg_id, answer),
            (false, _) => self.ack_authored(call, fx, leg_id, &None),
        }
    }

    /// ACK `leg_id`'s confirmed dialog on this stack's own account, bare
    /// (RFC 3261 §13.2.2.4): an offer of the dialog a reliable provisional
    /// carried is answered first, rejecting every stream, in the PRACK still
    /// owed (RFC 3262 §5), so the wire reads PRACK, ACK.
    pub(super) fn ack_own_bare(&mut self, call: &mut Call, fx: &mut HandlerEffects, leg_id: &str) {
        self.prack_deferred_offer(call, fx, leg_id, None);
        self.ack_leg(
            call,
            fx,
            leg_id,
            Vec::new(),
            None,
            Vec::new(),
            Provenance::Authored,
            relay::Author::Stack,
        );
    }

    /// ACK `leg_id`'s confirmed dialog on this stack's own account, answering
    /// the offer its 2xx made with every stream rejected (RFC 3264 §6), in the
    /// session this stack stated on the dialog (§8): no ACK carrying a peer's
    /// answer will come, and the dialog is BYEd next. Nothing when the ACK owes
    /// no answer.
    pub(super) fn ack_rejecting(&mut self, call: &mut Call, fx: &mut HandlerEffects, leg_id: &str) {
        let Some(leg) = call::helpers::find_leg(call, leg_id) else { return };
        let Some(offer) = relay::offer_owed_in_ack(leg).map(<[u8]>::to_vec) else { return };
        self.ack_rejecting_offer(call, fx, leg_id, &offer);
    }

    /// ACK `leg_id`'s confirmed dialog on this stack's own account, answering
    /// `offer`, the offer its 2xx made, with every stream rejected (RFC 3264
    /// §6), in the session this stack stated on the dialog (§8).
    pub(super) fn ack_rejecting_offer(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        offer: &[u8],
    ) {
        let Some(leg) = call::helpers::find_leg(call, leg_id) else { return };
        let origin = leg.dialogs.confirmed().and_then(|d| relay::next_origin_on(leg, d));
        let options = sip_message::BuildHeldSdpOptions {
            local_ip: self.config.sip_local_ip.clone(),
            now_ms: self.now_ms,
        };
        let answer = sip_message::sdp_answer::reject_offer(offer, &options)
            .and_then(|bytes| relay::in_dialog_session(bytes, origin.as_deref()))
            .unwrap_or_default();
        let content_type = (!answer.is_empty()).then(relay::sdp);
        self.ack_leg(
            call,
            fx,
            leg_id,
            answer,
            content_type,
            Vec::new(),
            Provenance::Authored,
            relay::Author::Stack,
        );
    }

    /// Whether `ack`, the caller's ACK of her initial INVITE, answers an offer
    /// made on her face alone: her INVITE made none, and `target_leg`'s 2xx
    /// made no offer its ACK answers (its INVITE carried the offer, or a
    /// reliable provisional did, RFC 3262 §5), so her description is no part
    /// of that leg's round (RFC 3264 §4).
    pub(super) fn answers_her_face_only(
        &mut self,
        call: &Call,
        target_leg: &str,
        ack: &sip_message::SipRequest,
    ) -> bool {
        let owes_an_answer = call::helpers::find_leg(call, target_leg)
            .and_then(|leg| leg.dialogs.established())
            .is_some_and(|d| matches!(d.owed_answer, Some(call::OwedAnswer::InAck { .. })));
        ack.sdp().is_some()
            && !owes_an_answer
            && self
                .caller_invite(call)
                .is_some_and(|c| c.cseq().seq() == ack.cseq().seq() && c.sdp().is_none())
    }

    /// The answer a relayed ACK of the caller's initial INVITE carrying none
    /// owes `target_leg`'s delayed offer ([`call::OwedAnswer::InAck`]), and
    /// whether it is the caller's: the offer answered out of her own
    /// description (her INVITE's offer, which the 2xx answered on her face),
    /// else every stream rejected on this stack's account (RFC 3264 §6). The
    /// ACK of a re-INVITE is relayed as it came.
    pub(super) fn answer_owed(
        &mut self,
        call: &Call,
        target_leg: &str,
        ack: &sip_message::SipRequest,
    ) -> Option<(Vec<u8>, bool)> {
        if ack.sdp().is_some() {
            return None;
        }
        let caller = self.caller_invite(call)?;
        if ack.cseq().seq() != caller.cseq().seq() {
            return None;
        }
        let leg = call.b_legs.iter().find(|l| l.leg_id == target_leg)?;
        let ack_cseq = ack.cseq().seq() as i64;
        let offer = match leg.dialogs.confirmed()?.owed_answer.as_ref()? {
            call::OwedAnswer::InAck { ack_cseq: owed, offer } if *owed == ack_cseq => offer.clone(),
            _ => return None,
        };
        let own = caller.sdp().and_then(|own| {
            sip_message::sdp_answer::answer_from_own(
                &offer,
                own,
                sip_message::sdp_answer::FormatPreference::Offerer,
                None,
            )
        });
        if let Some(answer) = own {
            return Some((answer, true));
        }
        let options = sip_message::BuildHeldSdpOptions {
            local_ip: self.config.sip_local_ip.clone(),
            now_ms: self.now_ms,
        };
        let rejected = sip_message::sdp_answer::reject_offer(&offer, &options)?;
        Some((rejected, false))
    }
}
