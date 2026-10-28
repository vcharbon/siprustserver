//! The transaction-scoped CANCEL of a relayed re-INVITE still pending toward
//! its target (RFC 3261 §9.1, §14.1): the dialogs stay as they were, only the
//! renegotiation ends — on the target's face by the CANCEL, on the
//! originator's by the transaction layer's 487 or this stack's own final.

use call::helpers::Scope;
use call::{Call, DialogId};
use sip_message::generators::{self, InviteClientTransactionHandle};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};

use super::select::find_pending_dialog;
use super::{ActionExecutor, Owed, Settle};

impl ActionExecutor<'_> {
    /// Transaction-scoped CANCEL of a relayed, still-pending **re-INVITE** on
    /// `leg_id`'s dialog (RFC 3261 §9.1). Unlike [`Self::cancel_to_leg`] /
    /// `CancelLeg` this touches NO leg state or disposition — the established
    /// dialog and the call stay up; only the renegotiation ends. The CANCEL is
    /// built from the INVITE the dialog's record holds, so it reuses
    /// the re-INVITE's branch, echoes its Route set (`generate_cancel`), and
    /// goes to the re-INVITE's cached wire destination — the same next-hop
    /// consistency as the initial-INVITE `cancel_to_leg` path. The matching
    /// pending-relay snapshot is marked `cancelled` so the target's eventual
    /// final (487, or a crossing 200) resolves locally instead of relaying.
    pub(super) fn cancel_pending_reinvite(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
    ) {
        let Some(cancel) = pending_reinvite_cancel(call, leg_id, outbound_cseq) else {
            return;
        };
        self.commit_pending_reinvite_cancel(call, fx, leg_id, outbound_cseq, cancel);
    }

    /// Put a built re-INVITE CANCEL on the wire and settle its books: the
    /// snapshot is marked `cancelled`, and the transaction's reliable
    /// provisionals end with the final the originator now holds — the
    /// transaction layer's 487 to a relayed CANCEL, or this stack's own
    /// reject (RFC 3262 §3).
    fn commit_pending_reinvite_cancel(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
        cancel: PendingReinviteCancel,
    ) {
        self.settle_owed_pracks(
            call,
            fx,
            Owed::Invite { leg: leg_id, cseq: outbound_cseq },
            Settle::Ending,
        );
        fx.outbound.push(cancel.effect);
        *call = call::helpers::cancel_relay(call.clone(), leg_id, &cancel.t_id, outbound_cseq);
        self.retire(call, fx, Scope::Transaction { leg_id, cseq: outbound_cseq });
    }

    /// End the relayed re-INVITE still pending on `leg_id`'s dialog under
    /// `outbound_cseq` on both faces, transaction-scoped (RFC 3261 §14.1 —
    /// the dialogs stay as they were): its originator is answered the locally
    /// authored `status` final, rebuilt from the pending snapshot the way a
    /// relayed final is (§8.2.6.2) and carrying nothing else — a failure
    /// states no body and no Contact — and the request toward the target is
    /// CANCELled as [`Self::cancel_pending_reinvite`] does. The two faces
    /// never disagree: the CANCEL is built before the final leaves, and where
    /// it cannot be built nothing is emitted, since a final the originator
    /// holds over a snapshot still live would let the target's own final be
    /// relayed as a second final on a completed transaction (RFC 3261
    /// §17.2.1). A snapshot already CANCELled has its originator's final from
    /// the transaction layer, so nothing is owed twice.
    pub(super) fn reject_pending_reinvite(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
        status: u16,
        reason: &str,
    ) {
        let Some((_, dialog)) = find_pending_dialog(call, leg_id, outbound_cseq) else {
            return;
        };
        let Some(pending) = call::helpers::find_relay(&dialog, outbound_cseq).cloned() else {
            return;
        };
        if pending.cancelled || pending.method != "INVITE" {
            return;
        }
        let refused = |what: &str| {
            tracing::warn!(
                call_ref = %call.call_ref,
                leg_id = %leg_id,
                status,
                "re-INVITE reject dropped — {what}"
            );
        };
        // §18.2.2: the final goes to the originator's top Via response
        // target; one no reader accepts names no destination (see
        // `relay_response`).
        let Some(dest) = pending
            .source_vias
            .first()
            .and_then(|v| super::snapshot_answer::via_response_target(v))
        else {
            refused("the originator's top Via does not read, so it names no destination");
            return;
        };
        let Some(cancel) = pending_reinvite_cancel(call, leg_id, outbound_cseq) else {
            refused("the pending re-INVITE toward the target cannot be CANCELled");
            return;
        };
        let Ok(reject) = super::snapshot_answer::stack_answer(&pending, "INVITE", status, reason)
        else {
            refused("the originator's request lines make no response");
            return;
        };
        let originator =
            call::helpers::get_peer(call, leg_id).unwrap_or(call.a_leg.leg_id.as_str()).to_string();
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Response(reject),
            mode: OutboundTxnMode::ServerResponse,
            destination: dest,
            label: format!("{status} INVITE → {originator}"),
            leg_id: Some(originator),
            provenance: Provenance::Authored,
        });
        self.commit_pending_reinvite_cancel(call, fx, leg_id, outbound_cseq, cancel);
    }
}

/// A re-INVITE CANCEL ready to leave, with the identity of the dialog whose
/// snapshot it settles.
struct PendingReinviteCancel {
    t_id: DialogId,
    effect: OutboundSipEffect,
}

/// Build the transaction-scoped CANCEL of the relayed re-INVITE pending on
/// `leg_id`'s dialog under `outbound_cseq` (RFC 3261 §9.1), from the INVITE
/// the dialog's record holds: the re-INVITE's own branch, Route set and wire
/// destination. `None` where no such transaction can be CANCELled — no
/// pending dialog, no image, an unreadable one, or one naming another CSeq
/// than the relay (the glare guard forbids a second in-flight INVITE on one
/// dialog, and a mismatched transaction is never CANCELled).
fn pending_reinvite_cancel(
    call: &Call,
    leg_id: &str,
    outbound_cseq: i64,
) -> Option<PendingReinviteCancel> {
    let (t_id, dialog) = find_pending_dialog(call, leg_id, outbound_cseq)?;
    let leg = call::helpers::find_leg(call, leg_id)?;
    let handle = call::helpers::sent_invite_image(leg, &dialog)?;
    let Ok(SipMessage::Request(req)) = CustomParser::new().parse(&handle.image) else {
        return None;
    };
    if req.cseq().seq() as i64 != outbound_cseq {
        return None;
    }
    let cancel =
        generators::generate_cancel(&InviteClientTransactionHandle { original_invite: req }, &[])
            .inspect_err(|e| crate::rules::relay::unbuilt(&call.call_ref, leg_id, "CANCEL", e))
            .ok()?;
    Some(PendingReinviteCancel {
        t_id,
        effect: OutboundSipEffect {
            body: OutboundBody::Request(cancel),
            mode: OutboundTxnMode::Raw,
            destination: (handle.destination.host.clone(), handle.destination.port),
            label: format!("CANCEL re-INVITE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
            provenance: Provenance::Authored,
        },
    })
}
