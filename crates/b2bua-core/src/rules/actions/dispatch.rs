//! The [`RuleAction`] → handler dispatch table. Simple state mutations apply
//! inline via the `call`-crate lens helpers; anything that builds SIP or spans
//! several steps delegates to its action-family module (relay / originate /
//! respond / dialog_track / teardown).

use call::helpers::RelayPeer;
use call::Call;
use std::ops::ControlFlow;

use crate::effects::{BufferedObservabilityEffect, HandlerEffects};
use crate::rules::relay;
use b2bua_sdk::model::{Body, MessageTransform, RuleAction, RuleContext};
use sip_message::SipHeader;

use super::select::resolve_peer;
use super::teardown::terminate_all;
use super::unrelayed::Unrelayed;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    /// Apply one action; `Break` ends the rule's list (a `CreateLeg` whose
    /// leg is refused).
    pub(super) fn step(
        &mut self,
        action: &RuleAction,
        ctx: &RuleContext,
        call: &mut Call,
        fx: &mut HandlerEffects,
    ) -> ControlFlow<()> {
        match action {
            RuleAction::CreateLeg {
                destination,
                new_ruri,
                new_from,
                new_to,
                callback_context,
                body_override,
                header_updates,
                header_adds,
                kind,
                attempt,
            } => self.create_leg(
                call,
                fx,
                ctx,
                destination,
                new_ruri.as_deref(),
                new_from.as_deref(),
                new_to.as_deref(),
                callback_context.as_deref(),
                body_override.as_ref(),
                header_updates,
                header_adds,
                *kind,
                attempt.as_ref(),
            ),
            _ => {
                self.apply(action, ctx, call, fx);
                ControlFlow::Continue(())
            }
        }
    }

    pub(super) fn apply(
        &mut self,
        action: &RuleAction,
        ctx: &RuleContext,
        call: &mut Call,
        fx: &mut HandlerEffects,
    ) {
        match action {
            // A leg the stepping refuses ends the list only when stepped.
            RuleAction::CreateLeg { .. } => {
                let _ = self.step(action, ctx, call, fx);
            }
            RuleAction::RelayToPeer { transform } => match resolve_peer(call, ctx) {
                RelayPeer::Leg { leg_id, dialog } => {
                    self.relay_to(call, fx, ctx, &leg_id, transform, dialog);
                }
                RelayPeer::NoLeg => self.refuse_unrelayed(call, fx, ctx, Unrelayed::NotNow),
                RelayPeer::Unadopted => {
                    self.refuse_unrelayed(call, fx, ctx, Unrelayed::ServiceOwned);
                }
            },
            RuleAction::RelayToLeg { leg_id, transform } => {
                let carrying = call::helpers::find_leg(call, leg_id)
                    .and_then(|l| l.dialogs.carrying())
                    .map(call::Dialog::id);
                self.relay_to(call, fx, ctx, leg_id, transform, carrying);
            }
            RuleAction::Respond { status, reason, body, content_type } => {
                self.respond(call, fx, ctx, *status, reason, body, content_type.as_deref(), vec![]);
            }
            RuleAction::RefuseGlare { refusal } => {
                self.refuse_glare(call, fx, ctx, *refusal);
            }
            RuleAction::AckLeg { leg_id, body } => self.ack_authored(call, fx, leg_id, body),
            RuleAction::AckLostRound { leg_id } => self.ack_lost_round(call, fx, ctx, leg_id),
            RuleAction::ReleaseStraggler2xx { leg_id } => {
                self.release_straggler(call, fx, ctx, leg_id);
            }
            RuleAction::ConfirmDialog { leg_id } => {
                self.confirm(call, fx, ctx, leg_id);
            }
            RuleAction::MapUnshownDialog { b_leg_id, b_tag } => {
                self.map_unshown_dialog(call, b_leg_id, b_tag);
            }
            RuleAction::DestroyLeg { leg_id, headers } => {
                self.destroy_leg(call, fx, ctx, leg_id, headers);
            }
            RuleAction::CancelLeg { leg_id } => {
                self.cancel_leg(call, fx, ctx, leg_id);
            }
            RuleAction::CancelPendingReinvite { leg_id, outbound_cseq } => {
                self.cancel_pending_reinvite(call, fx, leg_id, *outbound_cseq);
            }
            RuleAction::RejectPendingReinvite { leg_id, outbound_cseq, status, reason } => {
                self.reject_pending_reinvite(call, fx, leg_id, *outbound_cseq, *status, reason);
            }
            RuleAction::TerminateCall { cause, by_leg } => {
                self.reject_all_pending_non_invites(call, fx);
                terminate_all(call, self.now_ms, *cause, by_leg.clone());
            }
            RuleAction::BeginTermination { reason, cause, by_leg } => {
                self.begin_termination(call, fx, ctx, reason.as_deref(), (*cause, by_leg.clone()));
            }
            RuleAction::TerminateLeg { leg_id, end } => {
                self.terminate_leg(call, fx, ctx, leg_id, *end);
            }
            RuleAction::ReportStatedTextSkipped { detail } => {
                fx.buffered.push(BufferedObservabilityEffect::StatedTextSkipped {
                    detail: detail.clone(),
                });
            }
            RuleAction::Merge { .. }
            | RuleAction::Split { .. }
            | RuleAction::ResolveCancelledReinvite { .. }
            | RuleAction::ScheduleTimer { .. }
            | RuleAction::CancelTimer { .. }
            | RuleAction::CancelAllTimers
            | RuleAction::AddCdrEvent { .. }
            | RuleAction::MarkDecision { .. }
            | RuleAction::DeactivateRule { .. }
            | RuleAction::SetState { .. }
            | RuleAction::ClearState { .. }
            | RuleAction::SetPolicyUpdateBody { .. }
            | RuleAction::SetPromotePem { .. }
            | RuleAction::ReferAsyncHttp { .. }
            | RuleAction::ServiceHttpRequest { .. }
            | RuleAction::SetTransfer { .. }
            | RuleAction::FailureAsyncHttp { .. }
            | RuleAction::ReleaseAsyncHttp { .. }
            | RuleAction::ReplaceAdmissionSet { .. }
            | RuleAction::SetSubscriptions { .. }
            | RuleAction::SetReroute { .. }
            | RuleAction::SetFeatures { .. }
            | RuleAction::SetStatedHeaders { .. }
            | RuleAction::MergeCallExt { .. } => self.write_record(action, call, fx),
            RuleAction::SendRequestToLeg { leg_id, method, body, headers } => {
                let (bytes, content_type, author, descriptors) = parts(body);
                self.send_request_to_leg(
                    call,
                    fx,
                    leg_id,
                    method,
                    bytes,
                    content_type,
                    headers,
                    descriptors,
                    author,
                );
            }
            RuleAction::SendProvisionalToLeg {
                leg_id,
                status,
                reason,
                body,
                to_tag,
                p_early_media,
            } => {
                let (bytes, content_type, author, descriptors) = parts(body);
                self.send_provisional_to_leg(
                    call,
                    fx,
                    leg_id,
                    *status,
                    reason,
                    bytes,
                    content_type,
                    descriptors,
                    to_tag.as_deref(),
                    p_early_media.as_deref(),
                    author,
                );
            }
            RuleAction::TrackEarlyDialog { leg_id, b_tag } => {
                self.ensure_b_early_dialog(call, ctx, leg_id, b_tag);
            }
            RuleAction::CacheSdpOnLegDialog { leg_id, b_tag, body } => {
                // The suppressed fork's dialog registration: the cache
                // is keyed strictly on `(leg, b_tag)` — a fallback write would
                // overwrite a different fork's cached answer.
                self.ensure_b_early_dialog(call, ctx, leg_id, b_tag);
                *call = call::helpers::cache_sdp_on_leg_dialog(
                    call.clone(),
                    leg_id,
                    b_tag,
                    body.clone(),
                );
            }
            RuleAction::RelayFirstBare180 { leg_id, b_tag } => {
                self.relay_first_bare_180(call, fx, ctx, leg_id, b_tag);
            }
            RuleAction::SendReinvite { leg_id, body, add_headers } => {
                let (bytes, content_type, author, descriptors) = parts(body);
                self.send_reinvite(
                    call,
                    fx,
                    leg_id,
                    bytes,
                    content_type,
                    descriptors,
                    add_headers,
                    author,
                );
            }
            RuleAction::SendNotify { leg_id, event, subscription_state, content_type, body } => {
                self.send_notify(
                    call,
                    fx,
                    leg_id,
                    event,
                    subscription_state,
                    content_type.as_deref(),
                    body,
                );
            }
            RuleAction::RelayFailureToALeg { status, reason } => {
                self.relay_failure_to_a_leg(call, fx, ctx, *status, reason);
            }
            RuleAction::RespondToALeg { status, reason, header_updates, contacts } => {
                let authored = super::respond::AuthoredFinal {
                    status: *status,
                    reason,
                    header_updates,
                    contacts,
                };
                self.respond_to_a_leg(call, fx, ctx, authored);
            }
            RuleAction::AnswerALegNewDialog {
                status,
                reason,
                body,
                to_tag,
                header_updates,
                relayed,
            } => {
                let (bytes, content_type, author, descriptors) = parts(body);
                self.answer_a_leg_new_dialog(
                    call,
                    fx,
                    *status,
                    reason,
                    bytes,
                    content_type,
                    descriptors,
                    to_tag.as_deref(),
                    header_updates,
                    relayed,
                    author,
                );
            }
        }
    }

    /// Relay the current event to `target_leg` (response → regenerate on the
    /// a-leg; ACK → ack the target leg; other in-dialog request → regenerate on
    /// `target_dialog`, the dialog of the target leg it rides).
    pub(super) fn relay_to(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        transform: &MessageTransform,
        target_dialog: Option<call::DialogId>,
    ) {
        if let Some(resp) = ctx.response() {
            self.relay_response(call, fx, ctx, target_leg, transform, resp);
            return;
        }
        if let Some(req) = ctx.request() {
            self.relay_request(call, fx, ctx, target_leg, req, target_dialog);
        }
    }
}

/// A rule action's body as the executor sends it: bytes (empty for none), the
/// stated media type, its author, and the lines describing it.
pub(super) fn parts(body: &Option<Body>) -> (&[u8], Option<&str>, relay::Author<'_>, &[SipHeader]) {
    match body {
        Some(b) => {
            (&b.bytes, b.content_type.as_deref(), relay::Author::from(&b.author), &b.descriptors)
        }
        None => (&[], None, relay::Author::Stack, &[]),
    }
}
