//! Teardown: graceful call termination, per-leg destruction, and the BYE /
//! CANCEL builders (initial-INVITE CANCEL, transaction-scoped re-INVITE
//! CANCEL). The ADR-0022 unanswered-a-leg 503 does NOT live here — see
//! `crate::rules::invariants`.

use call::helpers::{
    set_bye_disposition, set_leg_disposition, set_leg_state, TERMINATING_TIMEOUT_MS,
};
use call::{ByeDisposition, Call, LegDisposition, LegState, StackDialog, TimerType};
use sip_message::generators::{
    self, GenerateInDialogRequestOpts, InDialogMethod, InviteClientTransactionHandle,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_txn::TxnKind;

use crate::effects::{HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode};
use crate::rules::relay;

use super::select::find_pending_dialog;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Graceful teardown. For every leg not already resolved — `terminated`,
    /// any `byeDisposition` already set (the firing rule pre-marked it, e.g.
    /// `bye_received`), or `cancelling` (a `cancel-leg` CANCEL is already in
    /// flight) — issue the right teardown: confirmed → BYE + `bye_sent`;
    /// trying/early b-leg → CANCEL + `cancelled` + terminated; trying/early
    /// a-leg → `none` (the rule already sent the SIP reply) — and, when a final
    /// to the a-leg is among this turn's outbound effects, → terminated too
    /// (the answered leg is resolved, so the ADR-0022 unanswered-a-leg
    /// invariant stays a pure safety net). Then enter `terminating` and arm the
    /// safety timer.
    ///
    /// `source_leg_id` is intentionally *not* special-cased here: rules that
    /// consume a BYE/CANCEL pre-mark their source leg's disposition before
    /// emitting begin-termination, so the skip guard below leaves it untouched.
    pub(super) fn begin_termination(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        _source_leg_id: &str,
        reason: Option<&str>,
    ) {
        // RFC 3326: stamp the teardown cause on each BYE only when the firing
        // rule supplied a structured `SIP;cause=…` value (the
        // `promote18xPemTo200` diagnostic teardown). The CORE rules pass opaque
        // labels ("BYE"/"CANCEL"/"481"); those are not RFC 3326 values and are
        // not emitted on the wire.
        let reason_header = reason.filter(|r| r.trim_start().starts_with("SIP"));
        // a-leg ∪ b-legs, in that order.
        let legs: Vec<(String, LegState, LegDisposition, Option<ByeDisposition>, bool)> =
            std::iter::once(&call.a_leg)
                .chain(call.b_legs.iter())
                .map(|l| (l.leg_id.clone(), l.state, l.disposition, l.bye_disposition, l.leg_id == call.a_leg.leg_id))
                .collect();
        for (id, state, disposition, bye_disposition, is_a) in legs {
            // Skip legs already handled by the firing rule or already resolved.
            if state == LegState::Terminated {
                continue;
            }
            if bye_disposition.is_some() {
                continue;
            }
            if disposition == LegDisposition::Cancelling {
                continue;
            }
            match state {
                LegState::Confirmed => {
                    let e = if is_a {
                        self.bye_to_leg_a(call, reason_header)
                    } else {
                        self.bye_to_b_leg(call, &id, reason_header)
                    };
                    if let Some(e) = e {
                        fx.outbound.push(e);
                    }
                    *call = set_bye_disposition(call.clone(), &id, ByeDisposition::ByeSent);
                }
                LegState::Trying | LegState::Early => {
                    if is_a {
                        // a-leg trying/early: the rule already sent the SIP reply.
                        *call = set_bye_disposition(call.clone(), &id, ByeDisposition::None);
                        // Parity with the b-leg arm: when that reply is REAL — a
                        // ≥200 final to the a-leg among THIS turn's outbound
                        // effects (`RespondToALeg` / `RelayFailureToALeg` are
                        // wire-only and never move leg state) — the leg is
                        // resolved; record it `Terminated` so a later turn's
                        // `→ terminated` edge doesn't read a still-`Early` a-leg
                        // and have the ADR-0022 invariant re-answer a spurious
                        // 503 (observed: a crossing BYE from the parked media
                        // leg right after the reject). A leg with NO final this
                        // turn deliberately stays Trying/Early —
                        // `answer_a_leg_if_unanswered` still owes that caller
                        // its 503.
                        let answered_this_turn = fx.outbound.iter().any(|e| {
                            e.leg_id.as_deref() == Some(id.as_str())
                                && matches!(&e.body, OutboundBody::Response(r) if r.status >= 200)
                        });
                        if answered_this_turn {
                            *call = set_leg_state(call.clone(), &id, LegState::Terminated);
                        }
                    } else {
                        if let Some(e) = self.cancel_to_leg(call, &id) {
                            fx.outbound.push(e);
                        }
                        *call = set_bye_disposition(call.clone(), &id, ByeDisposition::Cancelled);
                        // Same crossing-200 parity as `destroy_leg`: a ringing
                        // b-leg CANCELed by termination (e.g. `setup-timeout` +
                        // `BeginTermination` with no prior `CancelLeg`) must be
                        // `Cancelling` so a 200 racing the CANCEL is reaped by
                        // `cancel-200-crossing` rather than orphaning the callee.
                        *call = set_leg_disposition(call.clone(), &id, LegDisposition::Cancelling);
                        *call = set_leg_state(call.clone(), &id, LegState::Terminated);
                    }
                }
                LegState::Terminated => {}
            }
        }
        call.state = call::CallModelState::Terminating;
        self.schedule(call, fx, TimerType::TerminatingTimeout, TERMINATING_TIMEOUT_MS, None);
    }

    pub(super) fn destroy_leg(&self, call: &mut Call, fx: &mut HandlerEffects, leg_id: &str) {
        let state = call
            .b_legs
            .iter()
            .find(|l| l.leg_id == leg_id)
            .map(|l| l.state)
            .or_else(|| (call.a_leg.leg_id == leg_id).then_some(call.a_leg.state));
        match state {
            Some(LegState::Confirmed) => {
                if let Some(e) = self.bye_to_b_leg(call, leg_id, None) {
                    fx.outbound.push(e);
                }
                *call = set_bye_disposition(call.clone(), leg_id, ByeDisposition::ByeSent);
            }
            Some(LegState::Trying) | Some(LegState::Early) => {
                if let Some(e) = self.cancel_to_leg(call, leg_id) {
                    fx.outbound.push(e);
                }
                *call = set_bye_disposition(call.clone(), leg_id, ByeDisposition::Cancelled);
                // Parity with the explicit `CancelLeg` path: mark the ringing leg
                // `Cancelling` so a 200 OK that crosses this internally-originated
                // CANCEL on the wire matches `cancel-200-crossing` and is
                // ACK+BYE'd. Without this the late-answering callee is orphaned in
                // a one-sided established call (the 200 matches no rule — its
                // `confirm-dialog` needs Trying/Early state, which we just left).
                *call = set_leg_disposition(call.clone(), leg_id, LegDisposition::Cancelling);
            }
            _ => {}
        }
        *call = set_leg_state(call.clone(), leg_id, LegState::Terminated);
    }

    /// CANCEL a ringing b-leg and mark it `Cancelling`
    /// ([`crate::rules::model::RuleAction::CancelLeg`]).
    pub(super) fn cancel_leg(&self, call: &mut Call, fx: &mut HandlerEffects, leg_id: &str) {
        if let Some(e) = self.cancel_to_leg(call, leg_id) {
            fx.outbound.push(e);
        }
        *call = set_leg_disposition(call.clone(), leg_id, LegDisposition::Cancelling);
    }

    /// Bring `leg_id` to a terminal state
    /// ([`crate::rules::model::RuleAction::TerminateLeg`]).
    pub(super) fn terminate_leg(
        &self,
        call: &mut Call,
        leg_id: &str,
        bye_disposition: Option<ByeDisposition>,
    ) {
        *call = set_leg_state(call.clone(), leg_id, LegState::Terminated);
        if let Some(bd) = bye_disposition {
            *call = set_bye_disposition(call.clone(), leg_id, bd);
        }
        // Settle an in-flight CANCEL: a leg that TerminateLeg brings to a
        // terminal state is no longer "cancel pending", so clear the
        // `Cancelling` disposition that held it unresolved (see
        // `call::helpers::leg_is_resolved`). This is the seam the `487`
        // (`resolve-cancel-response`) and the force-terminal reaper /
        // safety-timeout paths ride to let a deferred termination finally
        // finalize; the crossing-`200` path clears `Cancelling` earlier via
        // `ConfirmDialog` (→ `Bridged`) instead.
        let cancelling = call::helpers::find_leg(call, leg_id)
            .map(|l| l.disposition == LegDisposition::Cancelling)
            .unwrap_or(false);
        if cancelling {
            *call = set_leg_disposition(call.clone(), leg_id, LegDisposition::Rejected);
        }
    }

    fn bye_to_b_leg(&self, call: &Call, leg_id: &str, reason: Option<&str>) -> Option<OutboundSipEffect> {
        let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
        let d = leg.dialogs.first()?;
        self.bye_on_dialog(&call.call_ref, leg_id, call.emergency == Some(true), &d.sip, reason)
    }

    fn bye_to_leg_a(&self, call: &Call, reason: Option<&str>) -> Option<OutboundSipEffect> {
        let d = call.a_leg.dialogs.first()?;
        self.bye_on_dialog(
            &call.call_ref,
            &call.a_leg.leg_id,
            call.emergency == Some(true),
            &d.sip,
            reason,
        )
    }

    fn bye_on_dialog(
        &self,
        call_ref: &str,
        leg_id: &str,
        is_emergency: bool,
        sip: &StackDialog,
        reason: Option<&str>,
    ) -> Option<OutboundSipEffect> {
        if sip.remote_tag.is_empty() {
            return None; // not a confirmed dialog
        }
        let dialog = relay::to_gen_dialog(sip);
        let branch = self.id_gen.new_branch();
        let extra_headers = reason
            .map(|r| {
                vec![sip_message::SipHeader {
                    name: "Reason".to_string().into(),
                    value: r.to_string().into(),
                }]
            })
            .unwrap_or_default();
        // Per-dialog CSeq (§12.2.1.1): `generate_in_dialog_request` defaults to
        // this dialog's `local_cseq + 1`, the next sequence number within THIS
        // dialog (a forked sibling's CSeq is irrelevant — distinct dialog).
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, call_ref, leg_id, is_emergency, branch)),
            extra_headers,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Bye, &dialog, &opts);
        let dest = relay::target_dest(&dialog.remote_target);
        let (req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &dialog.route_set, res.request, dest);
        Some(OutboundSipEffect {
            body: OutboundBody::Request(req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("BYE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        })
    }

    /// Transaction-scoped CANCEL of a relayed, still-pending **re-INVITE** on
    /// `leg_id`'s dialog (RFC 3261 §9.1). Unlike [`Self::cancel_to_leg`] /
    /// `CancelLeg` this touches NO leg state or disposition — the established
    /// dialog and the call stay up; only the renegotiation ends. The CANCEL is
    /// built from the dialog's cached `pending_invite_txn` handle, so it reuses
    /// the re-INVITE's branch, echoes its Route set (`generate_cancel`), and
    /// goes to the re-INVITE's cached wire destination — the same next-hop
    /// consistency as the initial-INVITE `cancel_to_leg` path. The matching
    /// pending-relay snapshot is marked `cancelled` so the target's eventual
    /// final (487, or a crossing 200) resolves locally instead of relaying.
    pub(super) fn cancel_pending_reinvite(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        outbound_cseq: i64,
    ) {
        let Some((t_id, dialog)) = find_pending_dialog(call, leg_id, outbound_cseq) else {
            return;
        };
        let Some(handle) = dialog.ext.pending_invite_txn.as_ref() else {
            return;
        };
        let Ok(SipMessage::Request(req)) = CustomParser::new().parse(&handle.original_invite)
        else {
            return;
        };
        // Defensive: the handle must be the re-INVITE this pending entry tracks
        // (the glare guard makes a second in-flight INVITE on this dialog
        // impossible, but never CANCEL a mismatched transaction).
        if req.cseq.seq as i64 != outbound_cseq {
            return;
        }
        let cancel = generators::generate_cancel(&InviteClientTransactionHandle {
            original_invite: req,
        });
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(cancel),
            mode: OutboundTxnMode::Raw,
            destination: (handle.destination.host.clone(), handle.destination.port),
            label: format!("CANCEL re-INVITE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
        *call =
            call::helpers::cancel_pending_request(call.clone(), leg_id, &t_id, outbound_cseq);
    }

    pub(super) fn cancel_to_leg(&self, call: &Call, leg_id: &str) -> Option<OutboundSipEffect> {
        let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
        let d = leg.dialogs.first()?;
        let handle = d.ext.pending_invite_txn.as_ref()?;
        let parsed = CustomParser::new().parse(&handle.original_invite).ok()?;
        let req = match parsed {
            SipMessage::Request(r) => r,
            _ => return None,
        };
        let cancel = generators::generate_cancel(&InviteClientTransactionHandle {
            original_invite: req,
        });
        // RFC 3261 §9.1: the CANCEL follows the INVITE's next hop, NOT `leg.source`
        // (the callee's advertised address). When the b-leg egresses through the
        // front proxy (`b2b_outbound_proxy`), the INVITE's wire destination — cached
        // on the txn handle at send — is the proxy; sending to `leg.source` would
        // bypass it, and the CANCEL would never reach the pending server txn the
        // proxy holds. The echoed Route set (above) keeps the CANCEL's path aligned
        // with the INVITE the whole way.
        Some(OutboundSipEffect {
            body: OutboundBody::Request(cancel),
            mode: OutboundTxnMode::Raw,
            destination: (handle.destination.host.clone(), handle.destination.port),
            label: format!("CANCEL → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        })
    }
}

/// Hard-terminate every leg and the call ([`crate::rules::model::RuleAction::TerminateCall`],
/// and the `CreateLeg` admission reject). No wire traffic — the firing rule owns
/// any final/BYE already sent.
pub(super) fn terminate_all(call: &mut Call) {
    *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Terminated);
    let ids: Vec<String> = call.b_legs.iter().map(|l| l.leg_id.clone()).collect();
    for id in ids {
        *call = set_leg_state(call.clone(), &id, LegState::Terminated);
    }
    call.state = call::CallModelState::Terminated;
}
