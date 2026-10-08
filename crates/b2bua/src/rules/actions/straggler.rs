//! The release of a fork STRAGGLER's 2xx (RFC 3261 §13.2.2.4): a 2xx to a
//! b-leg's INVITE under a To-tag no dialog of the leg carries, arriving once
//! another fork confirmed the leg. The UAC core ACKs every 2xx it receives and
//! BYEs a dialog it does not want, so this stack does both, once per straggler
//! dialog: a repeat of that 2xx is re-ACKed with the same ACK (same branch,
//! same bytes) and draws no second BYE. The call itself keeps the winner.

use call::helpers::set_leg_ext;
use call::{Call, Leg, StackDialog};
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

/// The leg ext slice the released stragglers live in: To-tag → the branch of
/// the ACK that released it.
const RELEASED: &str = "fork-straggler";

impl ActionExecutor<'_> {
    /// ACK the straggler 2xx the current event carries on `leg_id`, and BYE its
    /// dialog the first time it arrives.
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
        let Some(leg) = call.b_legs.iter().find(|l| l.leg_id == leg_id) else { return };
        let Some(base) = leg.dialogs.first() else { return };
        let released = released_branch(leg, &tag);
        let invite_cseq = resp.cseq().seq();
        let sip = StackDialog {
            remote_tag: tag.clone(),
            remote_target: contact_uri(resp.header::<header::Contact>(), &call.call_ref, leg_id)
                .unwrap_or_else(|| base.sip.remote_target.clone()),
            route_set: self.dialog_route_set(uac_route_set(resp), &call.call_ref, leg_id),
            local_cseq: i64::from(invite_cseq),
            ..base.sip.clone()
        };
        let marks = relay::CallMarks::of(call);
        let branch = released.clone().unwrap_or_else(|| self.id_gen.new_branch());
        let dialog = relay::to_gen_dialog(&sip);
        let opts = GenerateAckFor2xxOpts {
            via: Some(relay::leg_via(self.config, marks, leg_id, branch.clone())),
            cseq: Some(invite_cseq),
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
        let bye = self.bye_on_dialog(marks, leg_id, &sip, None, Relayed::none(), None);
        fx.outbound.extend(bye);
        let mut slice = leg
            .ext
            .as_ref()
            .and_then(|ext| ext.get(RELEASED))
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        slice.insert(tag, serde_json::Value::String(branch));
        *call = set_leg_ext(call.clone(), leg_id, RELEASED, serde_json::Value::Object(slice));
    }
}

/// The branch of the ACK that released the straggler `tag` on `leg`, where one
/// did.
fn released_branch(leg: &Leg, tag: &str) -> Option<String> {
    leg.ext.as_ref()?.get(RELEASED)?.get(tag)?.as_str().map(str::to_string)
}
