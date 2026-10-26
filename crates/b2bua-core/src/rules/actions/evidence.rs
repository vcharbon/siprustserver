//! Evidence the record cannot explain (ADR-0055 §4): what the message is owed
//! under RFC 3261, and the dialog state it proves taken into the record, so
//! the teardown that follows ends every dialog the record or the evidence
//! names. Construction, never repair: what the record holds of other dialogs
//! stays as it is.

use call::helpers::{caller_dialog, record_invite_final, update_leg};
use call::{Call, DialogState, Repeated, RetainedEmission};
use sip_message::{HeaderName, Method, SipRequest, SipResponse};

use crate::effects::{CriticalStateEffect, HandlerEffects, OutboundBody, Provenance};
use crate::rules::relay;
use b2bua_sdk::model::{LegEnd, RuleContext};

use super::dialog_track::open_early_a_dialog;
use super::ActionExecutor;

/// The status recorded for a non-2xx another owner sent the caller: her ACK
/// proves a final of 300 or more and names none.
const UNSEEN_REFUSAL: u16 = 500;

impl ActionExecutor<'_> {
    /// Owe the current message on `leg_id`, evidence the record cannot
    /// explain, what RFC 3261 requires of it, and take into the record the
    /// dialog state it proves
    /// ([`b2bua_sdk::model::RuleAction::EndOnEvidence`]).
    pub(super) fn end_on_evidence(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
    ) {
        if let Some(resp) = ctx.response() {
            if call::helpers::find_leg(call, leg_id).is_none() {
                self.release_unknown_leg_2xx(fx, resp, leg_id);
                let tag = resp.to().tag().unwrap_or_default();
                self.hold_possibly_confirmed(call, fx, leg_id, tag);
            } else {
                self.take_evidence_final(call, fx, ctx, leg_id, resp);
            }
            return;
        }
        match ctx.request() {
            Some(req) if *req.method() == Method::Ack => {
                self.take_evidence_ack(call, fx, ctx, leg_id, req)
            }
            // The caller's re-INVITE: another owner answered her INVITE; the
            // re-INVITE itself ends with the call (RFC 3261 §15.1.2).
            Some(req) if *req.method() == Method::Invite => {
                if let Some(tag) = req.to().tag() {
                    self.take_caller_answered(call, fx, leg_id, tag);
                }
                self.respond(call, fx, ctx, 487, "Request Terminated", &[], None, vec![]);
            }
            // The callee's BYE on an early dialog: it answered that dialog.
            Some(req) if *req.method() == Method::Bye => {
                let tag = req.from().tag().unwrap_or_default();
                self.hold_possibly_confirmed(call, fx, leg_id, tag);
            }
            _ => {}
        }
    }

    /// A final above its dialog's local sequence: the sequence is raised to
    /// it, so no later request re-uses a number the peer saw (RFC 3261
    /// §12.2.1.1); an INVITE final is ACKed, the round becoming the dialog's
    /// INVITE exchange, and one above the dialled INVITE's number on a dialog
    /// awaiting that INVITE's final (early, or its CANCEL out and crossed)
    /// proves that dialog confirmed and the dialled INVITE answered, so the
    /// teardown BYEs it instead of cancelling an answered INVITE; a BYE's
    /// final ends its dialog.
    fn take_evidence_final(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        resp: &SipResponse,
    ) {
        let Some(id) = ctx.dialog().map(call::Dialog::id) else { return };
        let cseq = i64::from(resp.cseq().seq());
        let invite = resp.cseq().method() == Method::Invite;
        *call = update_leg(call.clone(), leg_id, |leg| {
            // A re-INVITE rides a confirmed dialog (RFC 3261 §14).
            let reinvite = invite && leg.dialled_cseq().is_some_and(|dialled| cseq > dialled);
            if reinvite && leg.outbound_invite().is_some_and(|i| i.our_tag == id.our_tag) {
                if let Some(dialled) = leg.outbound_invite_mut() {
                    dialled.record_final(200);
                }
            }
            if let Some(d) = leg.dialogs.by_tags_mut(&id.our_tag, &id.peer_tag) {
                d.our_cseq = d.our_cseq.max(cseq);
                if invite {
                    d.sent_invite = None;
                }
                if reinvite && d.awaits_invite_final() {
                    d.state = DialogState::Confirmed;
                }
            }
        });
        match resp.cseq().method() {
            Method::Invite => self.ack_evidence_invite(call, fx, leg_id, &id, resp),
            Method::Bye => {
                super::dialog_end::end_dialogs(call, ctx, leg_id, LegEnd::OurByeAnswered)
            }
            _ => {}
        }
    }

    /// ACK the INVITE final `resp` on `leg_id`'s dialog `id`: a 2xx on this
    /// stack's own account, an offer it makes answered with every stream
    /// rejected (RFC 3264 §6), the ACK retained for a repeat (§13.2.2.4); a
    /// non-2xx hop by hop on its own branch, retained too: no transaction
    /// here absorbs a repeat of it (§17.1.1.2, §17.1.1.3).
    fn ack_evidence_invite(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        id: &call::DialogId,
        resp: &SipResponse,
    ) {
        let cseq = i64::from(resp.cseq().seq());
        if (200..300).contains(&resp.status()) {
            let options = sip_message::BuildHeldSdpOptions {
                local_ip: self.config.sip_local_ip.clone(),
                now_ms: self.now_ms,
            };
            // In the session this stack stated on the dialog (§8).
            let origin = call::helpers::find_leg(call, leg_id).and_then(|leg| {
                relay::next_origin_on(leg, leg.dialogs.by_tags(&id.our_tag, &id.peer_tag)?)
            });
            let answer = resp
                .sdp()
                .and_then(|offer| sip_message::sdp_answer::reject_offer(offer, &options))
                .and_then(|bytes| relay::in_dialog_session(bytes, origin.as_deref()))
                .unwrap_or_default();
            let content_type = (!answer.is_empty()).then(relay::sdp);
            // At the number the 2xx names: this side may have sent past it.
            self.ack_leg_at(
                call,
                fx,
                leg_id,
                Some(resp.cseq().seq()),
                (answer, content_type, Vec::new()),
                Provenance::Authored,
                relay::Author::Stack,
            );
            return;
        }
        let Some(ack) = call::helpers::find_leg(call, leg_id)
            .and_then(|leg| relay::ack_lost_non_2xx(&call.call_ref, leg, self.config, resp))
        else {
            return;
        };
        if let OutboundBody::Request(r) = &ack.body {
            let kept = RetainedEmission::on_trigger(
                r.image().to_vec(),
                ack.destination.clone(),
                Repeated::request("ACK"),
            );
            *call = call::helpers::retain_emitted_ack(call.clone(), leg_id, kept, cseq);
        }
        fx.outbound.push(ack);
    }

    /// A 2xx on a leg the record lacks, which its via-extension names: ACKed
    /// and its dialog BYEd from its own headers (RFC 3261 §13.2.2.4, §15.1.1).
    fn release_unknown_leg_2xx(
        &mut self,
        fx: &mut HandlerEffects,
        resp: &SipResponse,
        leg_id: &str,
    ) {
        let src = resp.top_via().sent_by().pair();
        let from = (src.0.to_string(), src.1);
        let answers = crate::unheld_response::answer_unheld_response(
            self.config,
            resp,
            from,
            &mut self.turn.ids,
            self.now_ms,
        );
        for mut answer in answers {
            answer.leg_id = Some(leg_id.to_string());
            fx.outbound.push(answer);
        }
    }
}

impl ActionExecutor<'_> {
    /// Another owner answered the caller's INVITE 2xx under `tag`: her INVITE
    /// is recorded answered and its server transaction here relinquished, so
    /// no final follows the 2xx, and the dialog `tag` names is confirmed.
    fn take_caller_answered(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        tag: &str,
    ) {
        if let Some(invite) = self.caller_invite(call) {
            let via = invite.top_via().clone();
            fx.critical.push(CriticalStateEffect::RelinquishAnsweredInvite { via });
        }
        *call = record_invite_final(call.clone(), leg_id, 200);
        if caller_dialog(call, tag).is_none() {
            let own = self.ensure_a_dialog_with(call, Some(tag.to_string()));
            if own != tag {
                open_early_a_dialog(call, tag);
            }
        }
        if let Some(d) = call::helpers::caller_dialog_mut(call, tag) {
            d.state = DialogState::Confirmed;
            d.shown = true;
        }
        // The 2xx ended every other early dialog of the caller's INVITE
        // (RFC 3261 §13.2.2.4).
        call::helpers::retire_early_dialogs(&mut call.a_leg);
    }

    /// Another owner answered the caller's INVITE `invite` with a non-2xx,
    /// whose ACK she sent: her INVITE is recorded refused, each of her early
    /// dialogs ended with it, and its server transaction here relinquished,
    /// so no final follows hers. No dialog of hers is left for a BYE.
    fn take_caller_refused(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        invite: &SipRequest,
    ) {
        let via = invite.top_via().clone();
        fx.critical.push(CriticalStateEffect::RelinquishAnsweredInvite { via });
        *call = record_invite_final(call.clone(), leg_id, UNSEEN_REFUSAL);
    }

    /// An ACK naming an INVITE the record holds unanswered: another owner
    /// answered it, 2xx unless the caller's ACK rides her INVITE's branch.
    /// Answered 2xx, the caller's INVITE is recorded answered and the
    /// dialog the ACK names confirmed under its tags; a re-INVITE's open
    /// round is taken as answered. The INVITE's server transaction here is
    /// relinquished, so no final follows the 2xx; nothing is sent for the ACK
    /// (RFC 3261 §17.1.1.3); the teardown BYEs the dialog.
    fn take_evidence_ack(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        req: &SipRequest,
    ) {
        let cseq = i64::from(req.cseq().seq());
        let caller_ack = req
            .to()
            .tag()
            .filter(|_| leg_id == call.a_leg.leg_id && cseq == i64::from(call.a_leg_invite.cseq));
        if let Some(tag) = caller_ack {
            match self.caller_invite(call) {
                // The ACK of a non-2xx rides the INVITE's own branch (RFC 3261
                // §17.1.1.3): her INVITE took a final that confirmed no dialog.
                Some(invite) if invite.top_via().branch() == req.top_via().branch() => {
                    self.take_caller_refused(call, fx, leg_id, &invite);
                }
                _ => self.take_caller_answered(call, fx, leg_id, tag),
            }
            return;
        }
        // A re-INVITE whose round the record holds open: its 2xx left with
        // another owner, so the teardown answers it nothing. A relayed one's
        // 2xx came from the peer, which is owed this very ACK (RFC 3261
        // §13.2.2.4): it goes on at the relayed INVITE's number, retained for
        // a repeat of that 2xx.
        let mut answered = Vec::new();
        let mut owed_ack = None;
        if let (Some(peer), Some(peer_leg)) = (
            ctx.peer_dialog().map(call::Dialog::id),
            call::helpers::get_peer(call, leg_id).map(str::to_string),
        ) {
            let relay = call::helpers::find_leg(call, &peer_leg)
                .and_then(|leg| leg.dialogs.by_tags(&peer.our_tag, &peer.peer_tag))
                .and_then(call::helpers::invite_relay)
                .filter(|r| r.inbound_cseq == cseq);
            if let Some(relay) = relay {
                answered.push(relay.clone());
                owed_ack = Some(peer_leg);
            }
        }
        if let Some(peer_leg) = owed_ack {
            let content_type = req.raw(HeaderName::ContentType).next().and_then(relay::media_type);
            self.ack_leg(
                call,
                fx,
                &peer_leg,
                req.body().to_vec(),
                content_type,
                Vec::new(),
                Provenance::Relayed,
                relay::Author::Leg(ctx.source_leg_id),
            );
        }
        if let Some(id) = ctx.dialog().map(call::Dialog::id) {
            *call = update_leg(call.clone(), leg_id, |leg| {
                if let Some(d) = leg.dialogs.by_tags_mut(&id.our_tag, &id.peer_tag) {
                    let held = |r: &call::RequestRecord| {
                        r.is_invite() && r.inbound_cseq == cseq && r.is_held()
                    };
                    answered.extend(d.requests.iter().filter(|r| held(r)).cloned());
                    d.requests.retain(|r| !held(r));
                }
            });
        }
        for record in answered {
            let via = record.source_vias.first().and_then(|v| {
                <sip_message::header::Via as sip_message::header::HeaderValue>::parse(
                    &sip_message::SipStr::owned(v),
                )
                .ok()
            });
            if let Some(via) = via {
                fx.critical.push(CriticalStateEffect::RelinquishAnsweredInvite { via });
            }
        }
    }
}
