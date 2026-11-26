//! The transaction-scoped CANCEL of a relayed re-INVITE still pending toward
//! its target (RFC 3261 §9.1, §14.1): the dialogs stay as they were. The
//! originator's own CANCEL is relayed on, and the target's final, whatever it
//! is, answers her (ADR-0065); this stack's reject answers her itself and
//! CANCELs toward the target.

use b2bua_sdk::model::RuleContext;
use call::helpers::Scope;
use call::{Call, DialogId};
use sip_message::generators::{self, InviteClientTransactionHandle};

use crate::effects::{
    HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode, Provenance,
};

use super::select::find_pending_dialog;
use super::{ActionExecutor, Owed, Settle};

impl ActionExecutor<'_> {
    /// Relay the originator's CANCEL of the relayed, still pending
    /// **re-INVITE** on `leg_id`'s dialog under `outbound_cseq` toward its
    /// target (RFC 3261 §9.1). Unlike [`Self::cancel_to_leg`] / `CancelLeg`
    /// this touches no leg state or disposition: the dialogs and the call stay
    /// up. The CANCEL is built from the INVITE the dialog's record holds, so it
    /// reuses the re-INVITE's branch, echoes its Route set (`generate_cancel`)
    /// and goes to its cached wire destination. The snapshot is marked
    /// `cancel_relayed`: the originator's INVITE stays open, and the target's
    /// final, a 487 or a 2xx that crossed the CANCEL, is relayed to her with
    /// the provisionals before it (ADR-0065). A CANCEL already relayed is not
    /// sent again.
    pub(super) fn cancel_pending_reinvite(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
    ) {
        let relayed = find_pending_dialog(call, leg_id, outbound_cseq)
            .and_then(|(_, d)| call::helpers::find_relay(&d, outbound_cseq).cloned())
            .is_some_and(|p| p.cancel_relayed);
        if relayed {
            return;
        }
        let notes = &mut self.turn.notes;
        let Some(cancel) = pending_reinvite_cancel(call, leg_id, outbound_cseq, notes) else {
            return;
        };
        fx.outbound.push(cancel.effect);
        call::helpers::relay_cancel(call, leg_id, &cancel.t_id, outbound_cseq);
    }

    /// Settle the books of a relayed re-INVITE whose originator now holds this
    /// stack's final, `cancel` the CANCEL toward the target still to leave:
    /// the snapshot is marked `cancelled`, and the transaction's reliable
    /// provisionals end with that final (RFC 3262 §3).
    fn end_pending_reinvite(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        (leg_id, t_id, outbound_cseq): (&str, &DialogId, i64),
        cancel: Option<OutboundSipEffect>,
    ) {
        self.settle_owed_pracks(
            call,
            fx,
            Owed::Invite { leg: leg_id, cseq: outbound_cseq },
            Settle::Ending,
        );
        fx.outbound.extend(cancel);
        call::helpers::cancel_relay(call, leg_id, t_id, outbound_cseq);
        self.retire(call, fx, Scope::Transaction { leg_id, cseq: outbound_cseq });
    }

    /// End the relayed re-INVITE still pending on `leg_id`'s dialog under
    /// `outbound_cseq` on both faces, transaction-scoped (RFC 3261 §14.1 —
    /// the dialogs stay as they were): its originator is answered the locally
    /// authored `status` final, rebuilt from the pending snapshot the way a
    /// relayed final is (§8.2.6.2) and carrying nothing else — a failure
    /// states no body and no Contact — and the request toward the target is
    /// CANCELled, unless the originator's own CANCEL already went there. The
    /// two faces never disagree: the CANCEL is built before the final leaves,
    /// and where it cannot be built nothing is emitted, since a final the
    /// originator holds over a snapshot still live would let the target's own
    /// final be relayed as a second final on a completed transaction (RFC 3261
    /// §17.2.1). A snapshot already `cancelled` has its originator's final, so
    /// nothing is owed twice.
    pub(super) fn reject_pending_reinvite(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
        status: u16,
        reason: &str,
    ) {
        let Some((t_id, dialog)) = find_pending_dialog(call, leg_id, outbound_cseq) else {
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
        let cancel = if pending.cancel_relayed {
            None
        } else {
            let notes = &mut self.turn.notes;
            let Some(cancel) = pending_reinvite_cancel(call, leg_id, outbound_cseq, notes) else {
                refused("the pending re-INVITE toward the target cannot be CANCELled");
                return;
            };
            Some(cancel.effect)
        };
        let Ok(reject) = super::snapshot_answer::stack_answer(&pending, "INVITE", status, reason)
        else {
            refused("the originator's request lines make no response");
            return;
        };
        let originator = super::settle::relay_originator(call, leg_id, &dialog.id());
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Response(reject),
            mode: OutboundTxnMode::ServerResponse,
            destination: dest,
            label: format!("{status} INVITE → {originator}"),
            leg_id: Some(originator),
            provenance: Provenance::Authored,
        });
        self.end_pending_reinvite(call, fx, (leg_id, &t_id, outbound_cseq), cancel);
    }

    /// Resolve the target's final `ctx` carries to the relayed re-INVITE on
    /// `leg_id`'s dialog under `outbound_cseq`, whose originator already holds
    /// a final ([`b2bua_sdk::model::RuleAction::ResolveCancelledReinvite`]):
    /// a 2xx is ACKed (RFC 3261 §13.2.2.4), answering an offer it made with
    /// every stream rejected (RFC 3264 §6), since no answer can come from the
    /// originator; the pending snapshot is dropped.
    pub(super) fn resolve_cancelled_reinvite(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        leg_id: &str,
        outbound_cseq: i64,
    ) {
        if let Some(resp) = ctx.response().filter(|r| (200..300).contains(&r.status())) {
            match crate::rules::relay::offer_of_2xx(call, leg_id, resp) {
                Some(offer) => self.ack_rejecting_offer(call, fx, leg_id, &offer),
                None => self.ack_own_bare(call, fx, leg_id),
            }
        }
        if let Some((t_id, _)) = find_pending_dialog(call, leg_id, outbound_cseq) {
            call::helpers::remove_relay(call, leg_id, &t_id, outbound_cseq);
        }
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
    notes: &mut crate::turn_notes::TurnNotes,
) -> Option<PendingReinviteCancel> {
    let (t_id, dialog) = find_pending_dialog(call, leg_id, outbound_cseq)?;
    let leg = call::helpers::find_leg(call, leg_id)?;
    let handle = call::helpers::sent_invite_image(leg, &dialog)?;
    let req = crate::stored_image::request_noted(&handle.image, call, "re-INVITE CANCEL", notes)?;
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
