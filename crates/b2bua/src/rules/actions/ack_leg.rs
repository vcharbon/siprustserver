//! ACK origination toward a leg (RFC 3261 §13.2.2.4): the ACK of a 2xx this
//! stack's INVITE drew, relayed from the peer or composed on its own account,
//! retained for the 2xx's retransmissions, and the answer a delayed offer is
//! owed in it (RFC 3264 §4).

use call::{Call, RetainedEmission};
use sip_message::header::MediaType;
use sip_message::SipHeader;

use crate::effects::{HandlerEffects, OutboundBody, Provenance, QuietTurn};
use crate::rules::relay;

use super::ActionExecutor;

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
        // The ACK rides the leg's dialog: the one a 2xx established.
        let dialog = call::helpers::find_leg(call, leg_id)
            .and_then(|l| l.dialogs.established())
            .map(|d| d.peer_tag.clone());
        let body = relay::continue_on_leg(
            call,
            leg_id,
            dialog.as_deref(),
            author,
            relay::Carried::InDialog,
            body,
            content_type.as_ref(),
            self.config.sdp_form.as_ref(),
        );
        let leg = if leg_id == call.a_leg.leg_id {
            Some(&call.a_leg)
        } else {
            call.b_legs.iter().find(|l| l.leg_id == leg_id)
        };
        // §13.2.2.4 hands THE ACK — the one the 2xx triggered — back to the
        // transport on every retransmitted copy: when this dialog's current
        // INVITE transaction already emitted its ACK, a bodyless ack re-sends
        // those bytes as they are (answer included on a delayed offer) instead
        // of composing a fresh, answerless datagram. A body-bearing call is a
        // first emission and composes below.
        if body.is_empty() {
            if let Some(retained) =
                leg.and_then(|l| l.dialogs.established()).and_then(|d| d.emitted_ack.as_ref())
            {
                fx.outbound.push(super::ladder::repeat_of(
                    retained,
                    format!("ACK (re-ACK, 2xx retransmit) → {leg_id}"),
                    leg_id,
                ));
                fx.quiet = Some(QuietTurn::ReAck);
                return;
            }
        }
        let ack = leg.and_then(|leg| {
            relay::ack_b_leg(
                relay::CallMarks::of(call),
                leg,
                self.config,
                &mut self.turn.ids,
                body,
                content_type,
                extra_headers,
                provenance,
            )
        });
        if let Some((e, branch)) = ack {
            // Retain THE ACK's exact bytes + destination before it leaves, so a
            // retransmitted 2xx re-passes this datagram (§13.2.2.4); the
            // end-of-turn stamp rebinds it to the bytes that leave.
            if let OutboundBody::Request(r) = &e.body {
                *call = call::helpers::retain_emitted_ack(
                    call.clone(),
                    leg_id,
                    RetainedEmission::on_trigger(
                        r.image().to_vec(),
                        e.destination.clone(),
                        call::Repeated::request(r.method().as_str()),
                    ),
                );
            }
            fx.outbound.push(e);
            // Retain the ACK branch so a retransmitted 2xx is re-ACKed on the
            // SAME transaction (§13.2.2.4 — `re-ack-retransmitted-2xx`).
            *call = call::helpers::retain_ack_branch(call.clone(), leg_id, &branch);
            // The ACK this dialog owed is on the wire, so the §13.2.2.4
            // obligation is discharged HERE — by the emission, whoever composed
            // it. A peer ACK for the same 2xx is then absorbed rather than
            // relayed on top of it (`relay_request`'s `owed` gate). A dialog
            // whose ACK nobody composes keeps the obligation and gets the peer's.
            *call = call::helpers::set_awaited_ack_cseq(call.clone(), leg_id, None);
            // A delayed offer's answer rode this ACK (RFC 3264 §4).
            *call = call::helpers::update_leg(call.clone(), leg_id, |l| {
                if let Some(d) = l.dialogs.established_mut() {
                    if matches!(d.owed_answer, Some(call::OwedAnswer::InAck { .. })) {
                        d.owed_answer = None;
                    }
                }
            });
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

    /// The answer a relayed ACK carrying none owes `target_leg`'s delayed offer
    /// ([`call::OwedAnswer::InAck`]), and whether it is the caller's: the
    /// offer answered out of her own description (her INVITE's offer, which
    /// the 2xx answered on her face), else every stream rejected on this
    /// stack's account (RFC 3264 §6).
    pub(super) fn answer_owed(
        &mut self,
        call: &Call,
        target_leg: &str,
        ack: &sip_message::SipRequest,
    ) -> Option<(Vec<u8>, bool)> {
        if ack.sdp().is_some() {
            return None;
        }
        let leg = call.b_legs.iter().find(|l| l.leg_id == target_leg)?;
        let ack_cseq = ack.cseq().seq() as i64;
        let offer = match leg.dialogs.confirmed()?.owed_answer.as_ref()? {
            call::OwedAnswer::InAck { ack_cseq: owed, offer } if *owed == ack_cseq => offer.clone(),
            _ => return None,
        };
        let caller = relay::rebuild_a_leg_invite(&call.a_leg_invite);
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
