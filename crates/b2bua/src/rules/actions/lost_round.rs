//! The acknowledgement of a round the record lost (ADR-0046): an INVITE final
//! on a confirmed dialog for a request this stack sent that no record here
//! holds — its transaction died with the node that sent it.

use call::helpers::update_leg;
use call::Call;

use crate::effects::{HandlerEffects, Provenance};
use crate::rules::relay;
use b2bua_sdk::model::RuleContext;

use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// ACK the INVITE final `ctx` carries on `leg_id`'s confirmed dialog. The
    /// dialog's sequence is raised to the round's first, so no later request
    /// reuses a number the peer saw (RFC 3261 §12.2.1.1). A 2xx makes the round
    /// the dialog's current INVITE transaction — the ACK of the earlier one
    /// retired — and is ACKed on this stack's own account, the ACK retained for
    /// a repeat (§13.2.2.4); a non-2xx is ACKed hop by hop on its own branch
    /// (§17.1.1.3).
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
                if answered {
                    d.pending_invite_txn = None;
                    d.ack_branch = None;
                    d.emitted_ack = None;
                }
            }
        });
        if answered {
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
            return;
        }
        let ack = call::helpers::find_leg(call, leg_id)
            .and_then(|leg| relay::ack_lost_non_2xx(leg, self.config, resp));
        fx.outbound.extend(ack);
    }
}
