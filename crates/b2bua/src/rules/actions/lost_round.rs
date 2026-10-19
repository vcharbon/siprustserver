//! The acknowledgement of a round the record lost (ADR-0047): an INVITE final
//! on a confirmed dialog for a request this stack sent that no record here
//! holds — its transaction died with the node that sent it.

use call::helpers::update_leg;
use call::{Call, Repeated, RetainedEmission};

use crate::effects::{HandlerEffects, OutboundBody, Provenance};
use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// ACK the INVITE final `ctx` carries on `leg_id`'s confirmed dialog. The
    /// dialog's sequence is raised to the round's first, so no later request
    /// reuses a number the peer saw (RFC 3261 §12.2.1.1), and the round becomes
    /// the dialog's current INVITE transaction — the ACK of the earlier one
    /// retired. A 2xx is ACKed on this stack's own account, an offer it makes
    /// answered with every stream rejected, the ACK retained for a repeat
    /// (§13.2.2.4); a non-2xx is ACKed hop by hop on its own branch, the ACK
    /// retained for a repeat as well (§17.1.1.2, §17.1.1.3).
    pub(super) fn ack_lost_round(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
    ) {
        let Some(resp) = ctx.response() else { return };
        let cseq = i64::from(resp.cseq().seq());
        let answered = (200..300).contains(&resp.status());
        *call = update_leg(call.clone(), leg_id, |leg| {
            if let Some(d) = leg.dialogs.established_mut() {
                d.our_cseq = d.our_cseq.max(cseq);
                d.pending_invite_txn = None;
                d.ack_branch = None;
                d.emitted_ack = None;
            }
        });
        if answered {
            // An offer in the 2xx is answered in the ACK (RFC 3264 §4), every
            // stream rejected (§6): the session it would describe is ended next.
            let options = sip_message::BuildHeldSdpOptions {
                local_ip: self.config.sip_local_ip.clone(),
                now_ms: self.now_ms,
            };
            let answer = resp
                .sdp()
                .and_then(|offer| sip_message::sdp_answer::reject_offer(offer, &options))
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
            return;
        }
        let Some(ack) = call::helpers::find_leg(call, leg_id)
            .and_then(|leg| relay::ack_lost_non_2xx(&call.call_ref, leg, self.config, resp))
        else {
            return;
        };
        // No transaction absorbs the final's repeats (§17.1.1.2): the record
        // keeps the ACK to send again (`re-ack-lost-round`).
        if let OutboundBody::Request(r) = &ack.body {
            let kept = RetainedEmission::on_trigger(
                r.image().to_vec(),
                ack.destination.clone(),
                Repeated::request("ACK"),
            );
            *call = call::helpers::retain_emitted_ack(call.clone(), leg_id, kept);
        }
        fx.outbound.push(ack);
    }
}
