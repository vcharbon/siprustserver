//! The [`RuleAction`] → handler dispatch table. Simple state mutations apply
//! inline via the `call`-crate lens helpers; anything that builds SIP or spans
//! several steps delegates to its action-family module (relay / originate /
//! respond / dialog_track / teardown).

use call::helpers::{
    add_cdr_event, add_tag_mapping, deactivate_rule, merge_leg, remove_pending_request,
    set_leg_disposition, set_leg_state, split_leg,
};
use call::{Call, CdrEvent, TagMapping};

use crate::effects::{CriticalStateEffect, HandlerEffects};
use crate::rules::model::{MessageTransform, RuleAction, RuleContext};
use crate::rules::relay;

use super::select::{find_pending_dialog, resolve_peer};
use super::teardown::terminate_all;
use super::ActionExecutor;

impl ActionExecutor<'_> {
    pub(super) fn apply(&self, action: &RuleAction, ctx: &RuleContext, call: &mut Call, fx: &mut HandlerEffects) {
        match action {
            RuleAction::RelayToPeer { transform } => {
                let (peer, target_to_tag) = resolve_peer(call, ctx);
                if let Some(peer) = peer {
                    self.relay_to(call, fx, ctx, &peer, transform, target_to_tag);
                }
            }
            RuleAction::RelayToLeg { leg_id, transform } => {
                self.relay_to(call, fx, ctx, leg_id, transform, None);
            }
            RuleAction::Respond {
                status,
                reason,
                body,
                content_type,
            } => {
                self.respond(fx, ctx, *status, reason, body, content_type.as_deref());
            }
            RuleAction::AckLeg { leg_id, body, content_type } => {
                // A body-bearing ACK carries a delayed-offer answer (RFC 3261
                // §13.2.2.4); default its type to `application/sdp` when none is
                // given. An empty ACK stays a bare ACK — no body, no Content-Type.
                let ct = if body.is_empty() {
                    None
                } else {
                    content_type
                        .as_deref()
                        .and_then(relay::media_type)
                        .or_else(|| Some(relay::sdp()))
                };
                self.ack_leg(call, fx, leg_id, body.clone(), ct);
            }
            RuleAction::ConfirmDialog { leg_id } => {
                self.confirm_dialog(call, ctx, leg_id);
            }
            RuleAction::UpdateLegState {
                leg_id,
                state,
                disposition,
            } => {
                *call = set_leg_state(call.clone(), leg_id, *state);
                if let Some(d) = disposition {
                    *call = set_leg_disposition(call.clone(), leg_id, *d);
                }
            }
            RuleAction::AddTagMapping {
                a_tag,
                b_leg_id,
                b_tag,
            } => {
                *call = add_tag_mapping(
                    call.clone(),
                    TagMapping {
                        a_tag: a_tag.clone(),
                        b_leg_id: b_leg_id.clone(),
                        b_tag: b_tag.clone(),
                    },
                );
            }
            RuleAction::Merge { leg_a, leg_b } => {
                *call = merge_leg(call.clone(), leg_a.clone(), leg_b.clone());
            }
            RuleAction::Split { leg_id } => {
                *call = split_leg(call.clone(), leg_id);
            }
            RuleAction::CreateLeg {
                destination,
                new_ruri,
                new_from,
                new_to,
                no_answer_timeout_sec,
                callback_context,
                body_override,
                header_updates,
                kind,
            } => {
                self.create_leg(
                    call,
                    fx,
                    ctx,
                    destination,
                    new_ruri.as_deref(),
                    new_from.as_deref(),
                    new_to.as_deref(),
                    *no_answer_timeout_sec,
                    callback_context.as_deref(),
                    body_override.as_deref(),
                    header_updates,
                    *kind,
                );
            }
            RuleAction::DestroyLeg { leg_id } => {
                self.destroy_leg(call, fx, leg_id);
            }
            RuleAction::CancelLeg { leg_id } => {
                self.cancel_leg(call, fx, ctx, leg_id);
            }
            RuleAction::CancelPendingReinvite { leg_id, outbound_cseq } => {
                self.cancel_pending_reinvite(call, fx, leg_id, *outbound_cseq);
            }
            RuleAction::ResolveCancelledReinvite { leg_id, outbound_cseq } => {
                // Drop the cancelled pending-relay snapshot: the final response
                // to the CANCELled relayed re-INVITE resolves here, never
                // relayed (the txn layer already 487'd the originator).
                if let Some((t_id, _)) = find_pending_dialog(call, leg_id, *outbound_cseq) {
                    *call = remove_pending_request(call.clone(), leg_id, &t_id, *outbound_cseq);
                }
            }
            RuleAction::ScheduleTimer {
                timer_type,
                delay_sec,
                leg_id,
            } => {
                self.schedule(call, fx, timer_type.clone(), delay_sec * 1000, leg_id.clone());
            }
            RuleAction::CancelTimer { id } => {
                call.timers.retain(|t| &t.id != id);
                fx.critical.push(CriticalStateEffect::CancelTimer { id: id.clone() });
            }
            RuleAction::CancelAllTimers => {
                call.timers.clear();
                fx.critical.push(CriticalStateEffect::CancelAllTimers);
            }
            RuleAction::TerminateCall => {
                terminate_all(call);
            }
            RuleAction::BeginTermination { reason } => {
                self.begin_termination(call, fx, ctx, reason.as_deref());
            }
            RuleAction::TerminateLeg {
                leg_id,
                bye_disposition,
            } => {
                self.terminate_leg(call, leg_id, *bye_disposition);
            }
            RuleAction::AddCdrEvent {
                event_type,
                leg_id,
                status_code,
                reason,
            } => {
                *call = add_cdr_event(
                    call.clone(),
                    CdrEvent {
                        event_type: *event_type,
                        timestamp: self.now_ms,
                        leg_id: leg_id.clone(),
                        status_code: *status_code,
                        reason: reason.clone(),
                    },
                );
            }
            RuleAction::DeactivateRule { rule_id } => {
                *call = deactivate_rule(call.clone(), rule_id);
            }
            RuleAction::SetState { machine, to } => {
                // The sole writer of `sm_cursors` (ADR-0016 X4). Transition
                // legality is enforced by the executor against the winning
                // rule's declared edges.
                call.sm_cursors.insert(machine.clone(), to.clone());
            }
            RuleAction::ClearState { machine } => {
                // Machine deactivation (ADR-0016 X9): remove the cursor, returning
                // the machine to dormant — the declarative inverse of `SetState`,
                // realising the transition to the terminal `[*]`. Idempotent.
                call.sm_cursors.remove(machine);
            }
            RuleAction::SendRequestToLeg {
                leg_id,
                method,
                body,
                content_type,
                headers,
            } => {
                self.send_request_to_leg(call, fx, leg_id, method, body, content_type.as_deref(), headers);
            }
            RuleAction::SendProvisionalToLeg {
                leg_id,
                status,
                reason,
                body,
                content_type,
                to_tag,
                p_early_media,
            } => {
                self.send_provisional_to_leg(
                    call,
                    fx,
                    leg_id,
                    *status,
                    reason,
                    body,
                    content_type.as_deref(),
                    to_tag.as_deref(),
                    p_early_media.as_deref(),
                );
            }
            RuleAction::SendPrackToLeg {
                leg_id,
                rseq,
                invite_cseq,
                b_tag,
            } => {
                // A *suppressed* fork's reliable 1xx never rode the relay path
                // (the only other early-dialog tracker), so register its
                // per-To-tag dialog here — the PRACK targets strictly `(leg,
                // b_tag)` (no first-dialog fallback).
                self.ensure_b_early_dialog(call, ctx, leg_id, b_tag);
                self.send_prack_to_leg(call, fx, leg_id, *rseq, *invite_cseq, b_tag);
            }
            RuleAction::CacheSdpOnLegDialog { leg_id, b_tag, body } => {
                // Same suppressed-fork registration as SendPrackToLeg: the cache
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
            RuleAction::SetPolicyUpdateBody { body } => {
                call.policy_update_body = Some(call::PolicyUpdateBody::Bytes(body.clone()));
            }
            RuleAction::RelayFirstBare180 { leg_id, b_tag } => {
                self.relay_first_bare_180(call, fx, ctx, leg_id, b_tag);
            }
            RuleAction::SendReinvite {
                leg_id,
                body,
                add_headers,
            } => {
                self.send_reinvite(call, fx, leg_id, body, add_headers);
            }
            RuleAction::SetPromotePem { state } => {
                *call = call::helpers::set_promote_pem(call.clone(), state.clone());
            }
            RuleAction::SendNotify {
                leg_id,
                event,
                subscription_state,
                content_type,
                body,
            } => {
                self.send_notify(call, fx, leg_id, event, subscription_state, content_type.as_deref(), body);
            }
            RuleAction::ReferAsyncHttp { request } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::ReferAsyncHttp {
                    call_ref: call.call_ref.clone(),
                    request: request.clone(),
                });
            }
            RuleAction::ServiceHttpRequest {
                correlation_id,
                endpoint,
                method,
                headers,
                body,
                content_type,
                timeout_ms,
            } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::ServiceHttpRequest {
                    call_ref: call.call_ref.clone(),
                    correlation_id: correlation_id.clone(),
                    endpoint: endpoint.clone(),
                    method: method.clone(),
                    headers: headers.clone(),
                    body: body.clone(),
                    content_type: content_type.clone(),
                    timeout_ms: *timeout_ms,
                });
            }
            RuleAction::SetTransfer { state } => {
                *call = call::helpers::set_transfer(call.clone(), state.clone());
            }
            RuleAction::FailureAsyncHttp { request } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::FailureAsyncHttp {
                    call_ref: call.call_ref.clone(),
                    request: request.clone(),
                });
            }
            RuleAction::ReleaseAsyncHttp { request } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::ReleaseAsyncHttp {
                    call_ref: call.call_ref.clone(),
                    request: request.clone(),
                });
            }
            RuleAction::SetSubscriptions { events } => {
                // The latest applied (re)route's declaration replaces the set
                // (decision-response parity with `SetFeatures`).
                call.subscriptions = events.clone();
            }
            RuleAction::SetReroute { state } => {
                *call = call::helpers::set_reroute(call.clone(), state.clone());
            }
            RuleAction::SetFeatures { features } => {
                call.features = Some(features.clone());
            }
            RuleAction::MergeCallExt { ext } => {
                for (service_id, value) in ext {
                    let v = (!value.is_null()).then(|| value.clone());
                    *call = call::helpers::set_call_ext(call.clone(), service_id, v);
                }
            }
            RuleAction::RecordLimiterHolds { entries, window } => {
                // Holds were INCRed by the router's failover fold; recording
                // them here is what makes the `→ terminated` invariant DECR
                // them (and the LimiterRefresh cadence re-stamp them).
                for (limiter_id, limit) in entries {
                    call.limiter_entries.push(call::CallLimiterState {
                        limiter_id: limiter_id.clone(),
                        limit: *limit,
                        origin_window: *window,
                        increment_succeeded: Some(true),
                    });
                }
            }
            RuleAction::RelayFailureToALeg { status, reason } => {
                self.relay_failure_to_a_leg(call, fx, ctx, *status, reason);
            }
            RuleAction::RespondToALeg { status, reason, header_updates, contacts } => {
                self.respond_to_a_leg(call, fx, ctx, *status, reason, header_updates, contacts);
            }
            RuleAction::AnswerALegNewDialog {
                status,
                reason,
                body,
                content_type,
                to_tag,
                header_updates,
            } => {
                self.answer_a_leg_new_dialog(
                    call,
                    fx,
                    *status,
                    reason,
                    body,
                    content_type.as_deref(),
                    to_tag.as_deref(),
                    header_updates,
                );
            }
            RuleAction::RetransmitALeg2xx => {
                self.retransmit_a_leg_2xx(call, fx);
            }
            RuleAction::RetransmitALegReinvite2xx => {
                self.retransmit_a_leg_reinvite_2xx(call, fx);
            }
            RuleAction::ClearPendingReinvite2xx => {
                if let Some(d) = call.a_leg.dialogs.first_mut() {
                    d.ext.pending_reinvite_2xx = None;
                }
            }
        }
    }

    /// Relay the current event to `target_leg` (response → regenerate on the
    /// a-leg; ACK → ack the target leg; other in-dialog request → regenerate on
    /// the peer dialog).
    pub(super) fn relay_to(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        transform: &MessageTransform,
        target_to_tag: Option<String>,
    ) {
        if let Some(resp) = ctx.response() {
            self.relay_response(call, fx, ctx, target_leg, transform, resp);
            return;
        }
        if let Some(req) = ctx.request() {
            self.relay_request(call, fx, ctx, target_leg, req, target_to_tag);
        }
    }
}
