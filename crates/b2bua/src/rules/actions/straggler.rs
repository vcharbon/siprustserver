//! The release of a fork STRAGGLER's 2xx (RFC 3261 §13.2.2.4): a 2xx to a
//! b-leg's INVITE under a To-tag no dialog of the leg carries, arriving once
//! another fork confirmed the leg. The UAC core ACKs every 2xx it receives and
//! BYEs a dialog it does not want, so this stack does both, once per straggler
//! dialog: a repeat of that 2xx is re-ACKed with the same ACK and draws no
//! second BYE. The release is booked apart from the winning dialog
//! ([`crate::rules::fork_straggler`]): its BYE continues the straggler's own
//! sequence, and that BYE's final and timeout are absorbed as the release's.

use call::{Call, StackDialog};
use sip_message::generators::{self, GenerateAckFor2xxOpts};
use sip_message::header;

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};
use crate::rules::fork_straggler::{Book, Release};
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
        &self,
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
        let Some(base) = leg.dialogs.first() else { return };
        let mut book = Book::of(leg);
        let released = book.released.get(&tag).cloned();
        let invite_cseq = resp.cseq().seq();
        let spent = book.forks.get(&tag).copied().unwrap_or(0);
        let sip = StackDialog {
            remote_tag: tag.clone(),
            remote_target: contact_uri(resp.header::<header::Contact>(), &call.call_ref, leg_id)
                .unwrap_or_else(|| base.sip.remote_target.clone()),
            route_set: self.dialog_route_set(uac_route_set(resp), &call.call_ref, leg_id),
            local_cseq: i64::from(invite_cseq).max(spent),
            ..base.sip.clone()
        };
        let offered_reliably = call::helpers::offered_in_reliable_provisional(
            call,
            leg_id,
            &tag,
            i64::from(invite_cseq),
        );
        let answer = (!relay::acked_invite_carries_offer(base) && !offered_reliably)
            .then(|| resp.sdp())
            .flatten()
            .and_then(|offer| {
                sip_message::sdp_answer::reject_offer(
                    offer,
                    &sip_message::BuildHeldSdpOptions {
                        local_ip: self.config.sip_local_ip.clone(),
                        now_ms: self.now_ms,
                    },
                )
            });
        let ack_branch = released
            .as_ref()
            .map(|r| r.ack_branch.clone())
            .unwrap_or_else(|| self.id_gen.new_branch());
        let dialog = relay::to_gen_dialog(&sip);
        let opts = GenerateAckFor2xxOpts {
            via: Some(relay::leg_via(self.config, marks, leg_id, ack_branch.clone())),
            cseq: Some(invite_cseq),
            content_type: answer.is_some().then(relay::sdp),
            body: answer.unwrap_or_default(),
            ..Default::default()
        };
        let ack = generators::generate_ack_for_2xx(None, &dialog, &opts);
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
        let Some(bye) = self.bye_on_dialog(marks, leg_id, &sip, None, Relayed::none(), None) else {
            return;
        };
        let bye_branch = match &bye.body {
            OutboundBody::Request(r) => r.top_via().branch().unwrap_or_default().to_string(),
            _ => String::new(),
        };
        fx.outbound.push(bye);
        book.released.insert(tag, Release { ack_branch, bye_branch });
        if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
            book.store(leg);
        }
    }
}
