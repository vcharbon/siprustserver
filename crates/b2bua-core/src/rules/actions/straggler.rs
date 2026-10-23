//! The release of a fork straggler's 2xx (RFC 3261 §13.2.2.4): a 2xx under a
//! To-tag no live dialog of an already confirmed b-leg carries is ACKed and
//! its dialog BYEd, once per straggler; a repeat is re-ACKed with the same ACK.
//! The straggler is a retired dialog of the leg ([`call::RetiredDialog`]): its
//! INVITE record keeps the ACK's branch, and the BYE, continuing its own
//! sequence, is a request record of its own until it settles (ADR-0055 §2).

use call::{Call, Dialog, DialogState, RetiredDialog};
use sip_message::generators::{self, GenerateAckFor2xxOpts};
use sip_message::header;

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

use super::dialog_track::{contact_uri, uac_route_set};
use super::teardown::Relayed;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// ACK the straggler 2xx the current event carries on `leg_id`, and BYE its
    /// dialog the first time it arrives.
    ///
    /// A delayed offer (§13.2.1: the leg's INVITE carried none) puts the offer
    /// in the 2xx, and the ACK MUST answer it (§13.2.2.4): the answer rejects
    /// every stream (RFC 3264 §6, port 0), the least a dialog about to be
    /// BYEd can commit to. Where a reliable provisional of the straggler's
    /// dialog carried the offer, its PRACK answered it and the ACK is bare
    /// (RFC 3262 §5).
    pub(super) fn release_straggler(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
    ) {
        let Some(resp) = ctx.response() else { return };
        let Some(tag) = resp.to().tag().filter(|t| !t.is_empty()).map(str::to_string) else {
            return;
        };
        let marks = relay::CallMarks::of(call);
        let Some(leg) = call.b_legs.iter().find(|l| l.leg_id == leg_id) else { return };
        let Some(invite) = leg.outbound_invite().cloned() else { return };
        let dialled = leg.dialled_on_dialog();
        let invite_cseq = resp.cseq().seq();
        // The losing fork's own record keeps the sequence it spent; a tag that
        // never rang starts from the INVITE (RFC 3261 §12.2.1.1).
        let mut straggler = match call::helpers::callee_dialog(leg, &tag) {
            Some(d) => d.clone(),
            None => {
                let mut d = Dialog::new(
                    invite.our_tag,
                    tag.clone(),
                    DialogState::Retired(RetiredDialog::default()),
                    i64::from(invite_cseq),
                );
                d.sent_invite = dialled;
                d
            }
        };
        straggler.our_cseq = straggler.our_cseq.max(i64::from(invite_cseq));
        if let Some(target) = contact_uri(resp.header::<header::Contact>(), &call.call_ref, leg_id)
        {
            straggler.remote_target = target;
        }
        straggler.route_set = self.dialog_route_set(uac_route_set(resp), &call.call_ref, leg_id);
        // Released already: its ACK left, its branch kept on the INVITE record.
        let released = call::helpers::ack_branch(&straggler).map(str::to_string);
        let offered_reliably = relay::offered_reliably(call, leg_id, &tag, i64::from(invite_cseq));
        let answer = (!relay::acked_invite_carries_offer(leg, &straggler) && !offered_reliably)
            .then(|| resp.sdp())
            .flatten()
            .and_then(|offer| {
                // In the session the fork's dialog carries: the INVITE's (§8).
                let origin = relay::next_origin_in_dialog(leg, &tag);
                sip_message::sdp_answer::reject_offer(
                    offer,
                    &sip_message::BuildHeldSdpOptions {
                        local_ip: self.config.sip_local_ip.clone(),
                        now_ms: self.now_ms,
                    },
                )
                .and_then(|bytes| relay::in_dialog_session(bytes, origin.as_deref()))
            });
        let ack_branch = released.clone().unwrap_or_else(|| self.turn.ids.new_branch());
        let dialog = relay::to_gen_dialog(leg, &straggler);
        let opts = GenerateAckFor2xxOpts {
            via: Some(relay::leg_via(self.config, marks, leg_id, ack_branch.clone())),
            cseq: Some(invite_cseq),
            content_type: answer.is_some().then(relay::sdp),
            body: answer.unwrap_or_default(),
            ..Default::default()
        };
        let ack = match generators::generate_ack_for_2xx(None, &dialog, &opts) {
            Ok(ack) => ack,
            Err(e) => return relay::unbuilt(&call.call_ref, leg_id, "ACK", &e),
        };
        let dest = relay::target_dest(&dialog.remote_target);
        let (ack, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &dialog.route_set, ack, dest);
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(ack),
            mode: OutboundTxnMode::Raw,
            destination: dest,
            label: format!("ACK (fork straggler {tag}) → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        });
        if released.is_some() {
            return;
        }
        let Some(bye) = self.bye_on_dialog(marks, leg, &straggler, None, Relayed::none(), None)
        else {
            return;
        };
        straggler.our_cseq += 1;
        let ack_cseq = i64::from(invite_cseq);
        let exchange = straggler
            .sent_invite
            .get_or_insert_with(|| call::RequestRecord::dialled_on_dialog(ack_cseq))
            .invite
            .get_or_insert_with(Default::default);
        exchange.ack_branch = Some(ack_branch);
        if !matches!(straggler.state, DialogState::Retired(_)) {
            straggler.state = DialogState::Retired(RetiredDialog::default());
        }
        let id = straggler.id();
        *call = call::helpers::update_leg(call.clone(), leg_id, |l| l.dialogs.insert(straggler));
        self.record_own_effect(call, leg_id, &id, &bye);
        fx.outbound.push(bye);
    }
}
