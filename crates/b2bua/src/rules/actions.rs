//! `ActionExecutor` — translates a rule's [`RuleAction`]s into a [`HandlerResult`]
//! (updated [`Call`] + typed [`HandlerEffects`]). Port of `ActionExecutor.ts`,
//! scoped to the basic-B2BUA action set. State mutations use the `call`-crate
//! lens helpers; outbound messages use the [`relay`] primitives.

use call::helpers::{
    add_b_leg, add_cdr_event, add_pending_request, add_tag_mapping, bump_local_cseq,
    deactivate_rule, find_by_b_tag, find_pending_request, merge_leg, relay_cseq_delta,
    remove_pending_request, replace_timer_by_id, set_bye_disposition, set_leg_disposition,
    set_leg_state, split_leg, update_remote_cseq, TERMINATING_TIMEOUT_MS,
};
use call::{
    B2buaDialogExt, ByeDisposition, Call, CdrEvent, Dialog, LegDisposition, LegState,
    PendingRequest, StackDialog, TagMapping, TimerEntry, TimerType,
};
use sip_message::generators::{
    self, GenerateInDialogRequestOpts, GenerateRelayedResponseOpts, GenerateResponseOpts,
    InDialogMethod, InviteClientTransactionHandle,
};
use sip_message::message_helpers::get_header;
use sip_message::parser::custom::structured_headers::split_top_level_commas;
use sip_message::parser::custom::CustomParser;
use sip_message::{Method, SipMessage, SipParser};
use sip_txn::{IdGen, TxnKind};

use crate::config::B2buaConfig;
use crate::effects::{
    CriticalStateEffect, HandlerEffects, HandlerResult, OutboundBody, OutboundSipEffect,
    OutboundTxnMode,
};

use super::model::{MessageTransform, RuleAction, RuleContext};
use super::relay;

/// Executes rule actions against a working copy of the call.
pub struct ActionExecutor<'a> {
    pub config: &'a B2buaConfig,
    pub id_gen: &'a IdGen,
    pub now_ms: i64,
}

impl<'a> ActionExecutor<'a> {
    /// Apply `actions` to a working copy of the authoritative `call`. The
    /// `ctx` view carries the event; the full struct comes in explicitly —
    /// rules never hold it (ADR-0020 X8).
    pub fn execute(
        &self,
        actions: &[RuleAction],
        call: &Call,
        ctx: &RuleContext,
    ) -> HandlerResult {
        let mut call = call.clone();
        let mut fx = HandlerEffects::new();
        for action in actions {
            self.apply(action, ctx, &mut call, &mut fx);
        }
        HandlerResult { call, effects: fx }
    }

    fn apply(&self, action: &RuleAction, ctx: &RuleContext, call: &mut Call, fx: &mut HandlerEffects) {
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
                if let Some(req) = ctx.request() {
                    let opts = GenerateResponseOpts {
                        body: body.clone(),
                        content_type: content_type.clone(),
                        ..Default::default()
                    };
                    let resp = generators::generate_response(req, *status, reason, &opts);
                    let dest = top_via_dest(req);
                    fx.outbound.push(OutboundSipEffect {
                        body: OutboundBody::Response(resp),
                        mode: OutboundTxnMode::ServerResponse,
                        destination: dest,
                        label: format!("{status} (respond)"),
                        leg_id: Some(ctx.source_leg_id.to_string()),
                    });
                }
            }
            RuleAction::AckLeg { leg_id } => {
                let leg = if leg_id == &call.a_leg.leg_id {
                    Some(&call.a_leg)
                } else {
                    call.b_legs.iter().find(|l| &l.leg_id == leg_id)
                };
                if let Some(leg) = leg {
                    if let Some(e) =
                        relay::ack_b_leg(&call.call_ref, leg, call.emergency == Some(true), self.config, self.id_gen, vec![], None)
                    {
                        fx.outbound.push(e);
                    }
                }
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
                // Admission gate — same policy as `apply_route`. A rule-driven
                // destination that doesn't pass the suffix allow-list is a config
                // bug; surface it as a terminate so the call doesn't hang waiting
                // for an answer that will never come (port of the `ActionExecutor.ts`
                // create-leg admission block). No leg / outbound is built; the call
                // is torn down and a `Reject` CDR records the cause (the Rust
                // analogue of the TS `admission_reject` span event — there is no
                // span-event channel in `HandlerEffects`).
                if crate::target_admission::classify_admission(
                    &destination.0,
                    &self.config.worker_allowed_target_suffixes,
                ) == crate::target_admission::AdmissionVerdict::Reject
                {
                    *call = add_cdr_event(
                        call.clone(),
                        CdrEvent {
                            event_type: call::CdrEventType::Reject,
                            timestamp: self.now_ms,
                            leg_id: ctx.source_leg_id.to_string(),
                            status_code: Some(503),
                            reason: Some(format!(
                                "admission_reject host={}",
                                destination.0
                            )),
                        },
                    );
                    terminate_all(call);
                    return;
                }
                let n = call.b_legs.len() + 1;
                let leg_id = format!("b-{n}");
                let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
                let (leg, effect) = relay::build_b_leg(
                    &call.call_ref,
                    &leg_id,
                    call.emergency == Some(true),
                    &a_invite,
                    destination.clone(),
                    new_ruri.as_deref(),
                    new_from.as_deref(),
                    new_to.as_deref(),
                    *no_answer_timeout_sec,
                    self.config,
                    self.id_gen,
                    body_override.as_deref(),
                    header_updates,
                    *kind,
                );
                if let Some(ctx_str) = callback_context {
                    call.callback_context = Some(ctx_str.clone());
                }
                *call = add_b_leg(call.clone(), leg);
                fx.outbound.push(effect);
                if let Some(secs) = no_answer_timeout_sec {
                    self.schedule(call, fx, TimerType::NoAnswer, secs * 1000, Some(leg_id));
                }
            }
            RuleAction::DestroyLeg { leg_id } => {
                self.destroy_leg(call, fx, leg_id);
            }
            RuleAction::CancelLeg { leg_id } => {
                if let Some(e) = self.cancel_to_leg(call, leg_id) {
                    fx.outbound.push(e);
                }
                *call = set_leg_disposition(call.clone(), leg_id, LegDisposition::Cancelling);
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
                self.begin_termination(call, fx, ctx.source_leg_id, reason.as_deref());
            }
            RuleAction::TerminateLeg {
                leg_id,
                bye_disposition,
            } => {
                *call = set_leg_state(call.clone(), leg_id, LegState::Terminated);
                if let Some(bd) = bye_disposition {
                    *call = set_bye_disposition(call.clone(), leg_id, *bd);
                }
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
            } => {
                self.send_request_to_leg(call, fx, leg_id, method, body, content_type.as_deref());
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
                self.send_prack_to_leg(call, fx, leg_id, *rseq, *invite_cseq, b_tag);
            }
            RuleAction::CacheSdpOnLegDialog { leg_id, b_tag, body } => {
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
                // Mint the a-facing To-tag (executor owns the IdGen), seed the
                // tag map for this b-leg dialog, record it, then relay the
                // current 1xx as a bare 180. The relay path resolves the
                // a-facing tag from the map (`find_by_b_tag`).
                let a_facing_tag = self.id_gen.new_tag();
                *call = add_tag_mapping(
                    call.clone(),
                    TagMapping {
                        a_tag: a_facing_tag.clone(),
                        b_leg_id: leg_id.clone(),
                        b_tag: b_tag.clone(),
                    },
                );
                *call = call::helpers::set_relay_first_18x_relayed(call.clone(), &a_facing_tag);
                let transform = MessageTransform {
                    status: Some(180),
                    reason: Some("Ringing".to_string()),
                    drop_body: true,
                    remove_headers: vec!["Require", "RSeq"],
                    add_headers: vec![],
                };
                let (peer, target_to_tag) = resolve_peer(call, ctx);
                if let Some(peer) = peer {
                    self.relay_to(call, fx, ctx, &peer, &transform, target_to_tag);
                }
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
            RuleAction::SetTransfer { state } => {
                *call = call::helpers::set_transfer(call.clone(), state.clone());
            }
            RuleAction::FailureAsyncHttp { request } => {
                fx.fire_and_forget.push(crate::effects::FireAndForgetEffect::FailureAsyncHttp {
                    call_ref: call.call_ref.clone(),
                    request: request.clone(),
                });
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
                let a_tag = self.ensure_a_dialog(call);
                let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
                let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
                fx.outbound.push(relay::response_to_a_leg(
                    &a_invite,
                    *status,
                    reason,
                    Some(a_tag),
                    Some(contact),
                    vec![],
                    None,
                    None,
                    vec![],
                ));
            }
            RuleAction::RespondToALeg { status, reason, header_updates, contacts } => {
                let a_tag = self.ensure_a_dialog(call);
                let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
                let extra = build_a_leg_response_headers(header_updates, contacts);
                // No B2BUA Contact: a redirect carries its own Contact list (via
                // `extra`), a reject carries none (ADR-0017 header-ownership X2).
                fx.outbound.push(relay::response_to_a_leg(
                    &a_invite,
                    *status,
                    reason,
                    Some(a_tag),
                    None,
                    vec![],
                    None,
                    None,
                    extra,
                ));
            }
            RuleAction::RetransmitALeg2xx => {
                self.retransmit_a_leg_2xx(call, fx);
            }
        }
    }

    /// RFC 3261 §13.3.1.4 — re-send the a-leg INVITE 2xx toward the caller while
    /// its ACK is missing. The a-leg INVITE server txn is already `Completed`, so
    /// the txn layer would DROP a second final on the `ServerResponse` path; we
    /// send a faithful copy **raw** instead (same confirmed To-tag, same cached
    /// answer SDP, the B2BUA Contact). No-op until the a-dialog is confirmed.
    fn retransmit_a_leg_2xx(&self, call: &Call, fx: &mut HandlerEffects) {
        let Some(d) = call.a_leg.dialogs.first() else { return };
        let a_tag = d.sip.local_tag.clone();
        if a_tag.is_empty() {
            return;
        }
        let body = d.ext.cached_sdp.clone().unwrap_or_default();
        let content_type = if body.is_empty() {
            None
        } else {
            Some("application/sdp".to_string())
        };
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        // A 2xx INVITE answer carries the B2BUA's own Allow/Supported (RFC 3261
        // §13.2.1/§20.37), exactly as the original confirm-dialog relay stamped —
        // so the retransmit is byte-faithful and the RFC audit stays clean.
        let mut extra: Vec<sip_message::SipHeader> = Vec::new();
        relay::stamp_a_facing_invite_advert(&mut extra, &[]);
        let mut effect = relay::response_to_a_leg(
            &a_invite,
            200,
            "OK",
            Some(a_tag),
            Some(contact),
            body,
            content_type,
            None,
            extra,
        );
        // Bypass the (Completed) a-leg server txn — it would drop a second final.
        effect.mode = OutboundTxnMode::Raw;
        effect.label = "200 (2xx retransmit, no ACK) → a-leg".to_string();
        fx.outbound.push(effect);
    }

    /// Originate a NOTIFY on `leg_id`'s confirmed dialog (toward the referrer)
    /// carrying the REFER implicit-subscription state (RFC 3515 §2.4.4): `Event:
    /// refer`, `Subscription-State`, and a `message/sipfrag` body. The B2BUA is
    /// the UAS of the referrer leg, so the NOTIFY rides that dialog's local
    /// sequence. Port of `executeSendNotify` (ActionExecutor.ts:2157).
    #[allow(clippy::too_many_arguments)]
    fn send_notify(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        event: &str,
        subscription_state: &str,
        content_type: Option<&str>,
        body: &[u8],
    ) {
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        let dialog = match leg_at(call, idx)
            .dialogs
            .iter()
            .find(|d| !d.sip.remote_tag.is_empty())
        {
            Some(d) => d.clone(),
            None => return,
        };
        let t_id = dialog_identity_tag(leg_id, &dialog);
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
            body: body.to_vec(),
            content_type: content_type.map(str::to_string),
            cseq: Some(outbound_cseq as u32),
            event: Some(event.to_string()),
            subscription_state: Some(subscription_state.to_string()),
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Notify, &gen_dialog, &opts);
        let dest = relay::dest_of(&relay::strip_uri(&gen_dialog.remote_target));
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("NOTIFY → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
    }

    /// Originate a re-INVITE on `leg_id` carrying `body` as the new offer plus
    /// `add_headers` (Allow/Supported). CSeq = dialog.localCSeq + 1. Used by
    /// `promote18xPemTo200` to resync Alice when bob's final SDP differs from the
    /// early-media SDP promoted into the synthetic 200 OK. The response comes back
    /// classified from-a (the B2BUA's stamped Via cr/lg) and is claimed by the
    /// `promote-resync-reinvite-response` rule.
    fn send_reinvite(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        body: &[u8],
        add_headers: &[(&'static str, String)],
    ) {
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        let dialog = match leg_at(call, idx)
            .dialogs
            .iter()
            .find(|d| !d.sip.remote_tag.is_empty())
        {
            Some(d) => d.clone(),
            None => return,
        };
        let t_id = dialog_identity_tag(leg_id, &dialog);
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let extra: Vec<sip_message::SipHeader> = add_headers
            .iter()
            .map(|(n, v)| sip_message::SipHeader {
                name: (*n).to_string(),
                value: v.clone(),
            })
            .collect();
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch.clone())),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
            body: body.to_vec(),
            content_type: (!body.is_empty()).then(|| "application/sdp".to_string()),
            cseq: Some(outbound_cseq as u32),
            extra_headers: extra,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Invite, &gen_dialog, &opts);
        let dest = relay::dest_of(&relay::strip_uri(&gen_dialog.remote_target));
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);

        // Cache the re-INVITE's client-transaction handle so the ACK-for-2xx
        // echoes its CSeq (§13.2.2.4).
        *call = call::helpers::update_dialog(call.clone(), leg_id, &t_id, |d| {
            d.ext.pending_invite_txn = Some(call::InviteTxnHandle {
                branch: branch.clone(),
                original_invite: sip_message::serialize(&SipMessage::Request(out_req.clone())),
                destination: call::HostPort { host: dest.0.clone(), port: dest.1 },
            });
        });

        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::Invite),
            destination: dest,
            label: format!("resync re-INVITE → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
    }

    /// Relay the current event to `target_leg` (response → regenerate on the
    /// a-leg; ACK → ack the b-leg; other in-dialog request → regenerate on the
    /// peer dialog). Port of `ActionExecutor.ts` `relayRequest` / `relayResponseMsg`.
    fn relay_to(
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

    /// Relay an inbound SIP request to `target_leg`. Replicates the source's
    /// per-dialog CSeq bookkeeping (`relayCSeqDelta` — each dialog has its own
    /// sequence, RFC 3261 §12.2.1.1), the PRACK `RAck` CSeq rewrite (RFC 3262
    /// §7.2), and the pending-request snapshot used to correlate the eventual
    /// response (RFC 3261 §8.1.3.3).
    fn relay_request(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        req: &sip_message::SipRequest,
        target_to_tag: Option<String>,
    ) {
        // ACK for 2xx: reuse the INVITE CSeq (no dialog-sequence advance,
        // §13.2.2.4) — delegate to the dedicated builder, carrying the inbound
        // ACK's body through (the delayed-offer re-INVITE answer rides the ACK,
        // RFC 3264 §4). The target may be either side (a re-INVITE answered by
        // bob is ACKed toward bob; one answered by alice is ACKed toward alice).
        if req.method == "ACK" {
            let leg = if target_leg == call.a_leg.leg_id {
                Some(&call.a_leg)
            } else {
                call.b_legs.iter().find(|l| l.leg_id == target_leg)
            };
            if let Some(leg) = leg {
                let content_type = get_header(&req.headers, "content-type").map(str::to_string);
                if let Some(e) = relay::ack_b_leg(
                    &call.call_ref,
                    leg,
                    call.emergency == Some(true),
                    self.config,
                    self.id_gen,
                    req.body.clone(),
                    content_type,
                ) {
                    fx.outbound.push(e);
                }
            }
            return;
        }
        let Some(method) = in_dialog_method(&req.method) else {
            return;
        };
        let Some(t_idx) = leg_index(call, target_leg) else {
            return;
        };
        // Forking: pick the early dialog by its callee tag (RFC 3261 §12.2.1.1 —
        // each forked early dialog is independent); else the first/only dialog.
        let target_dialog = {
            let leg = leg_at(call, t_idx);
            let picked = target_to_tag
                .as_deref()
                .and_then(|tt| leg.dialogs.iter().find(|d| d.sip.remote_tag == tt))
                .or_else(|| leg.dialogs.first());
            match picked {
                Some(d) => d.clone(),
                None => return,
            }
        };
        // A tag-less (mid-confirm / early) target dialog cannot produce a
        // well-formed in-dialog request — skip rather than panic in `make_request`
        // and leak the dialog. An X11 takeover/reclaim can surface a replica
        // dialog captured before its confirming To-tag landed.
        if target_dialog.sip.remote_tag.is_empty() {
            return;
        }

        // ── Per-dialog CSeq (§12.2.1.1): outbound = target.localCSeq + delta,
        //    delta = relayCSeqDelta(inbound, sourceDialog.remoteCSeq). ──
        let inbound_cseq = req.cseq.seq as i64;
        let source_leg_id = ctx.source_leg_id.to_string();
        let source_dialog = ctx.source_dialog().cloned();
        let source_remote_cseq = source_dialog.as_ref().and_then(|d| d.ext.remote_cseq);
        let delta = relay_cseq_delta(inbound_cseq, source_remote_cseq);
        let target_invite_cseq = invite_cseq_from_handle(&target_dialog)
            .unwrap_or(target_dialog.sip.local_cseq);
        let outbound_cseq = target_dialog.sip.local_cseq + delta;

        // Advance the sequences: source learns the inbound CSeq; target bumps.
        if let Some(sd) = &source_dialog {
            let s_id = dialog_identity_tag(&source_leg_id, sd);
            *call = update_remote_cseq(call.clone(), &source_leg_id, &s_id, inbound_cseq);
        }
        let t_id = dialog_identity_tag(target_leg, &target_dialog);
        *call = bump_local_cseq(call.clone(), target_leg, &t_id, delta);

        // RFC 3262 §7.2: rewrite RAck's middle (CSeq) token to the INVITE CSeq
        // that produced the reliable 1xx *on the target leg*.
        let rack = if method == InDialogMethod::Prack {
            get_header(&req.headers, "rack").map(|r| rewrite_rack(r, target_invite_cseq))
        } else {
            None
        };

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&target_dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, target_leg, call.emergency == Some(true), branch.clone())),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, target_leg, call.emergency == Some(true))),
            body: req.body.clone(),
            content_type: get_header(&req.headers, "content-type").map(str::to_string),
            rack,
            cseq: Some(outbound_cseq as u32),
            extra_headers: relay::relay_request_passthrough_headers(req),
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(method, &gen_dialog, &opts);
        let dest = relay::dest_of(&relay::strip_uri(&gen_dialog.remote_target));
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, target_leg, &gen_dialog.route_set, res.request, dest);
        let kind = if method == InDialogMethod::Invite {
            TxnKind::Invite
        } else {
            TxnKind::NonInvite
        };

        // For a re-INVITE, cache its client-transaction handle on the *target*
        // dialog so the eventual ACK-for-2xx echoes the re-INVITE CSeq
        // (RFC 3261 §13.2.2.4) and CANCEL can reuse the branch (§9.1). Port of
        // `relayRequest`'s `pendingInviteTxn` capture. (The initial INVITE never
        // reaches this path — it is built by `CreateLeg`/`build_b_leg`.)
        if method == InDialogMethod::Invite {
            *call = call::helpers::update_dialog(call.clone(), target_leg, &t_id, |d| {
                d.ext.pending_invite_txn = Some(call::InviteTxnHandle {
                    branch: branch.clone(),
                    original_invite: sip_message::serialize(&SipMessage::Request(out_req.clone())),
                    destination: call::HostPort { host: dest.0.clone(), port: dest.1 },
                });
            });
        }

        // Snapshot the inbound request so the response can echo its Via/From/To/
        // Call-ID/CSeq (§8.1.3.3) and so glare detection on the target dialog
        // sees the in-flight re-INVITE (`reinvite-glare`). The B2BUA answers BYE
        // locally and ACK has no response, so neither needs correlation.
        if !matches!(method, InDialogMethod::Bye) {
            let pending = PendingRequest {
                method: req.method.to_string(),
                outbound_cseq,
                inbound_cseq,
                source_vias: sip_message::message_helpers::get_headers(&req.headers, "via")
                    .iter()
                    .map(|s| s.to_string())
                    .collect(),
                source_call_id: req.call_id.clone(),
                source_from: get_header(&req.headers, "from").unwrap_or_default().to_string(),
                source_to: get_header(&req.headers, "to").unwrap_or_default().to_string(),
                direction: ctx.direction,
                cancelled: false,
            };
            *call = add_pending_request(call.clone(), target_leg, &t_id, pending);
        }

        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("relay {} → {target_leg}", req.method),
            leg_id: Some(target_leg.to_string()),
        });
    }

    /// Relay an inbound SIP response toward `target_leg` (normally the a-leg).
    /// Two paths, mirroring the source:
    ///   - **pending-correlated** (in-dialog non-INVITE: PRACK/OPTIONS/INFO/
    ///     UPDATE/…): rebuild from the snapshot captured when the request was
    ///     relayed, so the response echoes the caller's Via/From/To/CSeq.
    ///   - **default** (initial-INVITE 1xx/2xx): regenerate on the a-leg server
    ///     transaction, establishing the b-leg early dialog + a-facing tag map
    ///     on a reliable 1xx so a later PRACK can target the callee.
    fn relay_response(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        transform: &MessageTransform,
        resp: &sip_message::SipResponse,
    ) {
        let status = transform.status.unwrap_or(resp.status);
        let reason = transform.reason.clone().unwrap_or_else(|| resp.reason.clone());
        // The body relayed toward alice: dropped (bare-180 downgrade), replaced
        // by a staged policy body (fake-prack cached SDP on the 200 OK), or the
        // response's own body verbatim.
        let (relay_body, relay_content_type): (Vec<u8>, Option<String>) = if transform.drop_body {
            (vec![], None)
        } else if let Some(call::PolicyUpdateBody::Bytes(b)) = call.policy_update_body.clone() {
            (b, Some("application/sdp".to_string()))
        } else {
            (
                resp.body.clone(),
                get_header(&resp.headers, "content-type").map(str::to_string),
            )
        };
        // Passthrough headers minus any the transform suppresses (e.g.
        // Require/RSeq on a bare-180 downgrade), plus any the transform stamps
        // with replace semantics (Allow/Supported on the synthetic 200 / resync
        // re-INVITE, `promote18xPemTo200`).
        let add_headers = transform.add_headers.clone();
        let filter_passthrough = move |hs: Vec<sip_message::SipHeader>| -> Vec<sip_message::SipHeader> {
            let mut out: Vec<sip_message::SipHeader> = hs
                .into_iter()
                .filter(|h| {
                    !transform
                        .remove_headers
                        .iter()
                        .any(|r| r.eq_ignore_ascii_case(&h.name))
                        && !add_headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(&h.name))
                })
                .collect();
            for (name, value) in &add_headers {
                out.push(sip_message::SipHeader {
                    name: (*name).to_string(),
                    value: value.clone(),
                });
            }
            out
        };
        let cseq_num = resp.cseq.seq as i64;
        let cseq_method = if resp.cseq.method.as_str().is_empty() {
            "INVITE".to_string()
        } else {
            resp.cseq.method.to_string()
        };
        let to_tag = resp.to.tag.clone().unwrap_or_default();
        let source_leg_id = ctx.source_leg_id.to_string();

        // ── Pending transparent-relay correlation (§8.1.3.3) ──
        // Resolve the *exact* source dialog the response belongs to by its
        // To-tag (the responder's tag = this leg's dialog `remote_tag`). Under
        // forking (RFC 3261 §12.1.2) the source leg holds several early dialogs;
        // `source_dialog()` would return `dialogs.first()` (fork 1), so fork 2's
        // PRACK/UPDATE response would miss its pending entry and fall through to
        // the INVITE-response regeneration below — corrupting a `200 (PRACK)` /
        // `200 (UPDATE)` into a spurious `200 (INVITE)` toward the caller.
        let src_dialog = ctx
            .source_leg()
            .and_then(|leg| call::helpers::find_dialog_by_to_tag(leg, &to_tag))
            .or_else(|| ctx.source_dialog())
            .cloned();
        if let Some(src_dialog) = src_dialog {
            if let Some(pending) = find_pending_request(&src_dialog, cseq_num).cloned() {
                let contact = relay::leg_contact(self.config, &call.call_ref, target_leg, call.emergency == Some(true));
                let mut transparent_headers =
                    filter_passthrough(relay::relay_response_passthrough_headers(resp));
                // A 2xx answer to a B2BUA-relayed re-INVITE advertises the B2BUA's
                // own Allow/Supported toward the peer (RFC 3261 §13.2.1/§20.37),
                // replacing the source response's. Non-INVITE 2xx (PRACK/UPDATE)
                // and provisionals keep verbatim passthrough.
                if cseq_method == "INVITE" && (200..300).contains(&status) {
                    relay::stamp_a_facing_invite_advert(
                        &mut transparent_headers,
                        &transform.add_headers,
                    );
                }
                let opts = GenerateRelayedResponseOpts {
                    vias: pending.source_vias.clone(),
                    record_routes: vec![],
                    from: pending.source_from.clone(),
                    to: pending.source_to.clone(),
                    call_id: pending.source_call_id.clone(),
                    cseq: format!("{} {}", pending.inbound_cseq, cseq_method),
                    body: relay_body.clone(),
                    transparent_headers,
                    content_type: relay_content_type.clone(),
                    contact: Some(contact),
                };
                let relayed = generators::generate_relayed_response(status, &reason, &opts);
                let s_id = dialog_identity_tag(&source_leg_id, &src_dialog);
                // §8.1.3.3 / §14.1: the snapshot correlates EVERY response of the
                // transaction back to the originator, so it must outlive a relayed
                // provisional (1xx) — a re-INVITE may send 18x THEN a non-2xx final,
                // and dropping the snapshot on the 18x would orphan the final
                // (it would miss `relay-reinvite-response`, fall through to
                // `route-failure`, and wrongly tear the call down). Remove it only
                // on the FINAL (>= 200); the transaction has no later response then.
                if status >= 200 {
                    *call = remove_pending_request(call.clone(), &source_leg_id, &s_id, cseq_num);
                }
                let dest = pending
                    .source_vias
                    .first()
                    .map(|v| via_sent_by(v))
                    .unwrap_or_else(|| ("127.0.0.1".to_string(), 5060));
                fx.outbound.push(OutboundSipEffect {
                    body: OutboundBody::Response(relayed),
                    mode: OutboundTxnMode::ServerResponse,
                    destination: dest,
                    label: format!("{status} {cseq_method} → {target_leg}"),
                    leg_id: Some(target_leg.to_string()),
                });
                return;
            }
        }

        // ── b-leg INVITE 1xx/2xx → per-fork a-facing tag map ──
        // Each callee early dialog (forking → several per b-leg) gets its own
        // a-facing tag so the caller sees independent early dialogs; the response
        // is relayed under that per-fork tag (RFC 3261 §12; source confirm/relay).
        // The 2xx is included so that when a *non-first* fork wins, the confirmed
        // dialog the caller sees carries the WINNING fork's a-tag — not the first
        // fork's primary (RFC 3261 §13.2.2.4) — so the caller's ACK/in-dialog
        // requests address the dialog the B2BUA actually established.
        if cseq_method == "INVITE"
            && (100..300).contains(&resp.status)
            && !to_tag.is_empty()
            && source_leg_id != "a"
        {
            self.track_b_early_dialog(call, &source_leg_id, resp, &to_tag);
            let a_face = match find_by_b_tag(call, &source_leg_id, &to_tag) {
                Some(m) => m.a_tag.clone(),
                None => {
                    // First fork on this leg reuses the leg's primary a-tag (keeps
                    // a single confirmed a-dialog stable); later forks mint fresh.
                    let primary = self.ensure_a_dialog(call);
                    let a_face = if call.tag_map.iter().any(|m| m.b_leg_id == source_leg_id) {
                        self.id_gen.new_tag()
                    } else {
                        primary
                    };
                    *call = add_tag_mapping(
                        call.clone(),
                        TagMapping {
                            a_tag: a_face.clone(),
                            b_leg_id: source_leg_id.clone(),
                            b_tag: to_tag.clone(),
                        },
                    );
                    a_face
                }
            };
            let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
            let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
            let mut passthrough = filter_passthrough(relay::relay_response_passthrough_headers(resp));
            // A 2xx INVITE answer the B2BUA mints toward the caller advertises the
            // B2BUA's own capability set (RFC 3261 §13.2.1/§20.37), replacing any
            // Allow/Supported the callee's 200 carried. Provisionals keep verbatim
            // passthrough so reliable-1xx (Supported:100rel) negotiation survives.
            if (200..300).contains(&status) {
                relay::stamp_a_facing_invite_advert(&mut passthrough, &transform.add_headers);
            }
            let effect = relay::response_to_a_leg(
                &a_invite,
                status,
                &reason,
                Some(a_face),
                Some(contact),
                relay_body,
                relay_content_type,
                None,
                passthrough,
            );
            fx.outbound.push(effect);
            return;
        }

        // A non-INVITE response that reached here failed pending correlation
        // (§8.1.3.3): there is no a-leg transaction to answer. Regenerating it as
        // an INVITE response would emit a spurious `200 (INVITE)` toward the
        // caller (the exact forking corruption above). Drop it instead — only an
        // INVITE response legitimately regenerates on the a-leg server txn.
        if cseq_method != "INVITE" {
            return;
        }

        // ── Default: regenerate the INVITE response on the a-leg server txn ──
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        // Reliable-provisional negotiation headers (Require/Supported/RSeq) pass
        // through transparently so end-to-end PRACK keeps working (RFC 3262).
        let mut passthrough = filter_passthrough(relay::relay_response_passthrough_headers(resp));
        // A 2xx INVITE answer carries the B2BUA's own Allow/Supported, replacing
        // the callee's (RFC 3261 §13.2.1/§20.37); provisionals keep passthrough.
        if (200..300).contains(&status) {
            relay::stamp_a_facing_invite_advert(&mut passthrough, &transform.add_headers);
        }
        let effect = relay::response_to_a_leg(
            &a_invite,
            status,
            &reason,
            Some(a_tag),
            Some(contact),
            relay_body,
            relay_content_type,
            None,
            passthrough,
        );
        fx.outbound.push(effect);
    }

    /// Establish (or refresh) a b-leg early dialog from a reliable 1xx so a
    /// subsequent in-dialog request (PRACK/UPDATE) can target the callee with
    /// the right To-tag (RFC 3261 §12.1.2). Single early dialog per leg here;
    /// multi-fork early dialogs are a forking-slice concern.
    fn track_b_early_dialog(
        &self,
        call: &mut Call,
        source_leg_id: &str,
        resp: &sip_message::SipResponse,
        to_tag: &str,
    ) {
        let contact = get_header(&resp.headers, "contact").map(unwrap_angle).unwrap_or_default();
        // §12.1.2: an EARLY dialog's route set is established from the reliable
        // 1xx's Record-Route, exactly like the 2xx path below — split the
        // comma-combined double-record-route halves first, then reverse the
        // individual URIs (UAC side). Without this a PRACK/UPDATE on the early
        // dialog rides the preloaded bootstrap Route only and under-reproduces
        // the route set (the §12.2.1.1 audit catches it behind a front proxy).
        let mut early_route_set: Vec<String> =
            sip_message::message_helpers::get_headers(&resp.headers, "record-route")
                .iter()
                .flat_map(|h| split_top_level_commas(h))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        early_route_set.reverse();
        // The response echoes the INVITE's CSeq (§8.1.3.3); seed each forked
        // early dialog's sequence from it so they advance independently.
        let invite_cseq = resp.cseq.seq as i64;
        let already = call
            .b_legs
            .iter()
            .find(|l| l.leg_id == source_leg_id)
            .map(|l| l.dialogs.iter().any(|d| d.sip.remote_tag == to_tag))
            .unwrap_or(false);
        if already {
            return;
        }
        *call = call::helpers::update_leg(call.clone(), source_leg_id, |leg| {
            leg.state = LegState::Early;
            if let Some(d) = leg.dialogs.iter_mut().find(|d| d.sip.remote_tag.is_empty()) {
                // First real fork: seed the placeholder dialog in place so it
                // keeps its pending INVITE handle (ACK-for-2xx / RAck CSeq).
                d.sip.remote_tag = to_tag.to_string();
                if !contact.is_empty() {
                    d.sip.remote_target = contact.clone();
                }
                if !early_route_set.is_empty() {
                    d.sip.route_set = early_route_set.clone();
                }
            } else {
                // Additional fork: append a fresh independent early dialog
                // (RFC 3261 §12.1.2). All forks share the one INVITE the B2BUA
                // sent, so this dialog inherits the leg's initial-INVITE handle —
                // otherwise its 2xx ACK (and RAck CSeq) would fall back to the
                // running `local_cseq`, which any early PRACK/UPDATE has already
                // advanced past the INVITE (§13.2.2.4 wants the INVITE's CSeq).
                let leg_handle = leg
                    .dialogs
                    .iter()
                    .find_map(|d| d.ext.pending_invite_txn.clone());
                let ctx = call::helpers::MakeDialogLegCtx {
                    call_id: &leg.call_id,
                    local_uri: leg.local_uri.as_deref().unwrap_or(""),
                    remote_uri: leg.remote_uri.as_deref().unwrap_or(""),
                    local_tag: &leg.from_tag,
                    remote_tag: to_tag,
                };
                let mut d = call::helpers::make_empty_dialog(&ctx, invite_cseq);
                d.ext.pending_invite_txn = leg_handle;
                if !contact.is_empty() {
                    d.sip.remote_target = contact.clone();
                }
                if !early_route_set.is_empty() {
                    d.sip.route_set = early_route_set.clone();
                }
                leg.dialogs.push(d);
            }
        });
    }

    /// Confirm a b-leg dialog from the 2xx response event (learn remote tag /
    /// target / CSeq), mark it confirmed+bridged, and ensure the a-leg dialog
    /// exists.
    fn confirm_dialog(&self, call: &mut Call, ctx: &RuleContext, leg_id: &str) {
        let resp = match ctx.response() {
            Some(r) => r,
            None => return,
        };
        let remote_tag = resp.to.tag.clone().unwrap_or_default();
        let remote_tag_clone = remote_tag.clone();
        let remote_target = get_header(&resp.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_default();
        // §12.1.2: the b-leg is a UAC dialog, so its route set is the
        // dialog-creating 2xx's Record-Route values in *reverse* order (the
        // a-leg/UAS path keeps the INVITE's Record-Route forward). We must reverse
        // *individual route URIs*, not header lines: the front proxy double-record-
        // routes (a `;outbound` half + a cookie half), and on the wire those two
        // arrive comma-combined in a single Record-Route header (RFC 3261 §7.3.1).
        // Reversing per-header would be a no-op on that single value and leave the
        // cookie on top — so the worker→callee keepalive carries the cookie first,
        // the proxy decodes it (`w_pri`) and bounces the request back to a worker
        // after a reboot onto a new pod IP the registry has not yet learned (the
        // long-call-loss class). Split top-level commas first so the proxy's own
        // `;outbound` half lands on top and direction is intrinsic to its
        // Record-Route — no `;outbound` worker-stamp and no Via/registry rescue.
        let mut route_set: Vec<String> =
            sip_message::message_helpers::get_headers(&resp.headers, "record-route")
                .iter()
                .flat_map(|h| split_top_level_commas(h))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        route_set.reverse();
        if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
            // Forking (RFC 3261 §12.1.2): the 2xx confirms exactly ONE early
            // dialog — the one whose callee tag it carries. Promote *that* fork
            // (it already holds its own CSeq sequence, advanced by any early
            // PRACK/UPDATE) and discard the losing early dialogs. Confirming
            // `dialogs.first()` unconditionally would resurrect fork 1's stale
            // CSeq under fork 2's tag, so the next in-dialog request re-uses a
            // number the winning fork already spent (§12.2.1.1 violation).
            let idx = leg
                .dialogs
                .iter()
                .position(|d| !remote_tag.is_empty() && d.sip.remote_tag == remote_tag)
                .or_else(|| leg.dialogs.iter().position(|d| d.sip.remote_tag.is_empty()))
                .unwrap_or(0);
            if idx < leg.dialogs.len() {
                {
                    let d = &mut leg.dialogs[idx];
                    if !remote_tag.is_empty() {
                        d.sip.remote_tag = remote_tag;
                    }
                    if !remote_target.is_empty() {
                        d.sip.remote_target = remote_target;
                    }
                    if !route_set.is_empty() {
                        d.sip.route_set = route_set;
                    }
                    d.ext.remote_cseq = Some(resp.cseq.seq as i64);
                }
                // One dialog survives confirmation (model: "one survives after
                // confirmed") — drop the losing forks so per-call state is bounded.
                let winner = leg.dialogs.remove(idx);
                leg.dialogs = vec![winner];
            }
            leg.state = LegState::Confirmed;
            leg.disposition = LegDisposition::Bridged;
        }
        // Reuse the a-facing tag pre-seeded for this callee (relayFirst18x's
        // `force-tag-consistency`) so the 200 OK To-tag matches the first 180.
        let preferred = find_by_b_tag(call, leg_id, &remote_tag_clone).map(|m| m.a_tag.clone());
        self.ensure_a_dialog_with(call, preferred.clone());
        // When a *non-first* fork wins, the a-dialog was already created under
        // the first fork's primary tag; adopt the winning fork's a-face tag so
        // the confirmed a-dialog matches the To-tag the caller saw on the 2xx
        // (and any B2BUA-originated a-facing in-dialog request uses it).
        if let (Some(pref), Some(d)) = (preferred, call.a_leg.dialogs.first_mut()) {
            if !pref.is_empty() {
                d.sip.local_tag = pref;
            }
        }
        // Stash the answer SDP that was relayed toward alice on the 2xx so an
        // un-ACKed-2xx retransmit (RFC 3261 §13.3.1.4) re-sends a faithful copy —
        // the caller that lost the original 200 needs its answer. Mirror the relay
        // body choice (policy override else the callee's 200 body).
        let answer_body = match call.policy_update_body.clone() {
            Some(call::PolicyUpdateBody::Bytes(b)) => Some(b),
            _ if !resp.body.is_empty() => Some(resp.body.clone()),
            _ => None,
        };
        if let (Some(body), Some(d)) = (answer_body, call.a_leg.dialogs.first_mut()) {
            d.ext.cached_sdp = Some(body);
        }
        *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Confirmed);
    }

    /// Ensure the a-leg has a dialog with a stable B2BUA-minted local tag; return
    /// that tag (the To-tag presented to alice on every a-facing response).
    fn ensure_a_dialog(&self, call: &mut Call) -> String {
        self.ensure_a_dialog_with(call, None)
    }

    /// Like [`ensure_a_dialog`] but, when the a-dialog is being created, uses
    /// `preferred` as its local tag instead of minting a fresh one (tag
    /// continuity across forking/failover, `relayFirst18xTo180`).
    fn ensure_a_dialog_with(&self, call: &mut Call, preferred: Option<String>) -> String {
        if let Some(d) = call.a_leg.dialogs.first() {
            if !d.sip.local_tag.is_empty() {
                return d.sip.local_tag.clone();
            }
        }
        let tag = preferred.unwrap_or_else(|| self.id_gen.new_tag());
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let remote_target = get_header(&a_invite.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_else(|| a_invite.from.uri.clone());
        // §12.1.1: the a-leg is a UAS dialog — route set is the INVITE's
        // Record-Route values in forward order. Split top-level commas so a
        // comma-combined header (the proxy's double-record-route halves) becomes
        // individual route URIs, same as the b-leg path above.
        let route_set: Vec<String> =
            sip_message::message_helpers::get_headers(&a_invite.headers, "record-route")
                .iter()
                .flat_map(|h| split_top_level_commas(h))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        let dialog = Dialog {
            sip: StackDialog {
                call_id: call.a_leg.call_id.clone(),
                local_tag: tag.clone(),
                remote_tag: call.a_leg.from_tag.clone(),
                local_uri: a_invite.to.uri.clone(),
                remote_uri: a_invite.from.uri.clone(),
                remote_target,
                local_cseq: a_invite.cseq.seq as i64,
                route_set,
            },
            ext: B2buaDialogExt {
                remote_cseq: Some(a_invite.cseq.seq as i64),
                inbound_pending_requests: vec![],
                ack_branch: None,
                pending_invite_txn: None,
                cached_sdp: None,
            },
        };
        call.a_leg.dialogs = vec![dialog];
        tag
    }

    fn schedule(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        timer_type: TimerType,
        delay_ms: i64,
        leg_id: Option<String>,
    ) {
        // The ONE persisted-id recipe (`TimerType::timer_id`); every cancel
        // site mints from the same method so schedule/cancel can never drift.
        let id = timer_type.timer_id(leg_id.as_deref());
        let entry = TimerEntry {
            id,
            timer_type,
            fire_at: self.now_ms + delay_ms,
            leg_id,
        };
        call.timers = replace_timer_by_id(std::mem::take(&mut call.timers), entry.clone());
        fx.critical.push(CriticalStateEffect::ScheduleTimer(entry));
    }

    /// Graceful teardown (port of `executeBeginTermination`). For every leg not
    /// already resolved — `terminated`, any `byeDisposition` already set (the
    /// firing rule pre-marked it, e.g. `bye_received`), or `cancelling` (a
    /// `cancel-leg` CANCEL is already in flight) — issue the right teardown:
    /// confirmed → BYE + `bye_sent`; trying/early b-leg → CANCEL + `cancelled` +
    /// terminated; trying/early a-leg → `none` (the rule already sent the SIP
    /// reply). Then enter `terminating` and arm the safety timer.
    ///
    /// `source_leg_id` is intentionally *not* special-cased here: rules that
    /// consume a BYE/CANCEL pre-mark their source leg's disposition before
    /// emitting begin-termination, so the skip guard below leaves it untouched.
    fn begin_termination(
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
        // not emitted on the wire (matches the prior behaviour for them).
        let reason_header = reason.filter(|r| r.trim_start().starts_with("SIP"));
        // a-leg ∪ b-legs, in that order (mirrors the TS leg iteration).
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

    fn destroy_leg(&self, call: &mut Call, fx: &mut HandlerEffects, leg_id: &str) {
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

    #[allow(clippy::too_many_arguments)]
    fn send_request_to_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        method: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) {
        let m = match in_dialog_method(&Method::from_wire(method)) {
            Some(m) => m,
            None => return,
        };
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        // Probe the CONFIRMED dialog (non-empty remote tag). A leg can hold an
        // early/forked dialog with no remote tag as its first entry, and a
        // failover takeover/reclaim (ADR-0011 X11) can materialise a replica
        // dialog captured mid-confirm; building an in-dialog `To` from an empty
        // remote tag yields a tag-less header that panics in `make_request` and
        // leaks the dialog. Skip when no dialog is confirmed (nothing to probe).
        let dialog = match leg_at(call, idx)
            .dialogs
            .iter()
            .find(|d| !d.sip.remote_tag.is_empty())
        {
            Some(d) => d.clone(),
            None => {
                // An EARLY/forked or mid-confirm leg legitimately has no confirmed
                // dialog yet — skip quietly. But a CONFIRMED (established) leg whose
                // dialog carries no remote tag is a broken invariant that should be
                // UNREACHABLE: an established dialog ALWAYS knows its peer's tag
                // (b-leg ← callee 2xx To-tag; a-leg ← caller INVITE From-tag, stored
                // in the call context and hydrated verbatim), and a tag-less INVITE
                // is now rejected with 400 at ingest (router::process), so the a-leg
                // remote tag can never be seeded empty. If we still land here the
                // dialog became tag-less by some OTHER path (a takeover/reclaim
                // replica captured mid-confirm, a tag dropped on a state mutation),
                // which silently drops every in-dialog request to this leg — the
                // keepalive OPTIONS never fires for it, so we poke the peer but not
                // this side ("OPTIONS to called, not calling"). Never swallow it.
                let leg = leg_at(call, idx);
                if leg.state == LegState::Confirmed {
                    eprintln!(
                        "B2BUA INVARIANT VIOLATION: call_ref={} leg={} is Confirmed but has \
                         no dialog with a remote tag ({} dialog(s), all tag-less) — cannot \
                         originate in-dialog {} (keepalive will never fire for this leg). \
                         A tag-less INVITE is rejected at ingest, so an established dialog \
                         must never reach this state; the call context preserves the empty \
                         tag across hydration, leaving this leg permanently un-probeable.",
                        call.call_ref,
                        leg_id,
                        leg.dialogs.len(),
                        method,
                    );
                }
                return;
            }
        };
        // Advance + persist the dialog CSeq by exactly one (§12.2.1.1), exactly as
        // every other in-dialog originator here (send_with_body / send_reinvite /
        // send_prack_to_leg). Without this the in-dialog keepalive OPTIONS
        // re-derives the same CSeq every cycle and the later relayed BYE collides
        // on it — an RFC 3261 violation a real UAS rejects (endurance
        // long-call-loss class).
        let t_id = dialog_identity_tag(leg_id, &dialog);
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        // Opaque body carrier (MSCML INFO rides here): default the content type
        // to `application/sdp` when a body is present and none was given (port of
        // the source's `contentType ?? (body ? "application/sdp")`).
        let content_type = content_type
            .map(str::to_string)
            .or_else(|| (!body.is_empty()).then(|| "application/sdp".to_string()));
        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
            cseq: Some(outbound_cseq as u32),
            body: body.to_vec(),
            content_type,
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(m, &gen_dialog, &opts);
        let dest = relay::dest_of(&relay::strip_uri(&gen_dialog.remote_target));
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);
        let kind = if m == InDialogMethod::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("{method} → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
    }

    /// Broker an unadopted leg's SDP onto the a-leg as an unreliable provisional
    /// (RFC 3262 §3 early media). Port of `executeSendProvisionalToLeg`. Only the
    /// a-leg has a stored UAS INVITE to answer; a non-a target or a non-1xx status
    /// is skipped. `to_tag` set ⇒ ephemeral forked early dialog (verbatim, not
    /// persisted); absent ⇒ the B2BUA's own early identity (reuse/mint+persist).
    #[allow(clippy::too_many_arguments)]
    fn send_provisional_to_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
        to_tag: Option<&str>,
        p_early_media: Option<&str>,
    ) {
        if !(100..200).contains(&status) || leg_id != call.a_leg.leg_id {
            return;
        }
        // `to_tag` provided → an ephemeral forked early dialog, used verbatim and
        // NOT persisted onto the a-dialog. Absent → the B2BUA's own early identity:
        // reuse the existing a-dialog tag or mint and persist one.
        let to_tag = match to_tag {
            Some(t) => t.to_string(),
            None => self.ensure_a_dialog(call),
        };
        // SDP early-media body defaults to application/sdp (mirrors the request path).
        let content_type = content_type
            .map(str::to_string)
            .or_else(|| (!body.is_empty()).then(|| "application/sdp".to_string()));
        let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id, call.emergency == Some(true));
        let mut extra_headers = Vec::new();
        if let Some(pem) = p_early_media {
            extra_headers.push(sip_message::SipHeader {
                name: "P-Early-Media".to_string(),
                value: pem.to_string(),
            });
        }
        fx.outbound.push(relay::response_to_a_leg(
            &a_invite,
            status,
            reason,
            Some(to_tag),
            Some(contact),
            body.to_vec(),
            content_type,
            None,
            extra_headers,
        ));
    }

    /// Originate a PRACK toward the b-leg early dialog (selected by callee tag)
    /// acknowledging a reliable 1xx (RFC 3262 §4). The RAck is
    /// `<rseq> <invite_cseq> INVITE`; the dialog's local CSeq advances by one.
    /// Used by `relayFirst18xTo180` (B2BUA PRACKs bob since alice never saw the
    /// reliable provisional).
    fn send_prack_to_leg(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        rseq: i64,
        invite_cseq: i64,
        b_tag: &str,
    ) {
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        // Pick the early dialog by callee tag (forking → independent dialogs).
        let dialog = {
            let leg = leg_at(call, idx);
            let picked = leg
                .dialogs
                .iter()
                .find(|d| d.sip.remote_tag == b_tag)
                .or_else(|| leg.dialogs.first());
            match picked {
                Some(d) => d.clone(),
                None => return,
            }
        };
        // No confirmed remote tag (an early/mid-confirm dialog, e.g. a takeover
        // replica captured before its provisional To-tag landed) → skip rather
        // than build a tag-less in-dialog PRACK and panic in `make_request`.
        if dialog.sip.remote_tag.is_empty() {
            return;
        }
        let t_id = dialog_identity_tag(leg_id, &dialog);
        // Per-dialog CSeq (§12.2.1.1): each forked early dialog has its OWN
        // sequence, so its first PRACK is the INVITE CSeq + 1 independent of the
        // other forks — two forks' PRACKs at the same CSeq is correct (distinct
        // dialogs, distinct To-tags), not a collision.
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, call.emergency == Some(true), branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id, call.emergency == Some(true))),
            rack: Some(format!("{rseq} {invite_cseq} INVITE")),
            cseq: Some(outbound_cseq as u32),
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(InDialogMethod::Prack, &gen_dialog, &opts);
        let dest = relay::dest_of(&relay::strip_uri(&gen_dialog.remote_target));
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &gen_dialog.route_set, res.request, dest);
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(TxnKind::NonInvite),
            destination: dest,
            label: format!("PRACK → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
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
                    name: "Reason".to_string(),
                    value: r.to_string(),
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
        let dest = relay::dest_of(&relay::strip_uri(&dialog.remote_target));
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
    fn cancel_pending_reinvite(
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

    fn cancel_to_leg(&self, call: &Call, leg_id: &str) -> Option<OutboundSipEffect> {
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

fn terminate_all(call: &mut Call) {
    *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Terminated);
    let ids: Vec<String> = call.b_legs.iter().map(|l| l.leg_id.clone()).collect();
    for id in ids {
        *call = set_leg_state(call.clone(), &id, LegState::Terminated);
    }
    call.state = call::CallModelState::Terminated;
}

/// The peer of `leg_id`: the other side of the active pair, else a↔(first b-leg).
/// Dialog identity tag: a-leg → its local (B2BUA) tag; b-leg → the remote
/// (callee) tag (port of `dialogIdentityTag`). Selects the dialog within a leg.
fn dialog_identity_tag(leg_id: &str, dialog: &Dialog) -> String {
    if leg_id == "a" {
        dialog.sip.local_tag.clone()
    } else {
        dialog.sip.remote_tag.clone()
    }
}

/// Find the dialog on `leg_id` (the a-leg or a b-leg) holding the pending
/// transparent-relay snapshot for `outbound_cseq`; returns its identity tag
/// (dialog selector for the lens helpers) plus an owned clone of the dialog.
fn find_pending_dialog(call: &Call, leg_id: &str, outbound_cseq: i64) -> Option<(String, Dialog)> {
    let leg = if call.a_leg.leg_id == leg_id {
        &call.a_leg
    } else {
        call.b_legs.iter().find(|l| l.leg_id == leg_id)?
    };
    leg.dialogs
        .iter()
        .find(|d| find_pending_request(d, outbound_cseq).is_some())
        .map(|d| (dialog_identity_tag(leg_id, d), d.clone()))
}

/// The INVITE CSeq cached on a dialog's pending INVITE handle (RFC 3261
/// §13.2.2.4 / RFC 3262 §7.2). Parses the snapshot; `None` if absent/unparseable.
fn invite_cseq_from_handle(dialog: &Dialog) -> Option<i64> {
    let handle = dialog.ext.pending_invite_txn.as_ref()?;
    match CustomParser::new().parse(&handle.original_invite).ok()? {
        SipMessage::Request(r) => Some(r.cseq.seq as i64),
        _ => None,
    }
}

/// Rewrite the middle (CSeq) token of an `RAck` (`<rseq> <cseq> <method>`) to
/// `cseq`, preserving RSeq + method (RFC 3262 §7.2).
fn rewrite_rack(rack: &str, cseq: i64) -> String {
    let parts: Vec<&str> = rack.split_whitespace().collect();
    if parts.len() == 3 {
        format!("{} {} {}", parts[0], cseq, parts[2])
    } else {
        rack.to_string()
    }
}

/// Parse a Via header's sent-by `host:port` (RFC 3261 §18.2.2) for response
/// routing.
fn via_sent_by(via: &str) -> (String, u16) {
    via.split_whitespace()
        .nth(1)
        .and_then(|s| s.split(';').next())
        .map(|hp| relay::dest_of(hp.trim()))
        .unwrap_or_else(|| ("127.0.0.1".to_string(), 5060))
}

/// Resolve the relay target leg plus, for forking, the specific callee early-
/// dialog tag. Thin view over [`call::helpers::resolve_relay_peer`] — the ONE
/// resolver this relay path shares with the rule-vocabulary readiness predicate
/// (`RuleContext::peer_relay_ready`, GAP-P8b-2), so the two never drift.
fn resolve_peer(call: &Call, ctx: &RuleContext) -> (Option<String>, Option<String>) {
    let to_tag = ctx.request().and_then(|r| r.to.tag.as_deref());
    call::helpers::resolve_relay_peer(call, ctx.source_leg_id, to_tag)
}

fn leg_index(call: &Call, leg_id: &str) -> Option<usize> {
    if leg_id == call.a_leg.leg_id {
        Some(usize::MAX)
    } else {
        call.b_legs.iter().position(|l| l.leg_id == leg_id)
    }
}

fn leg_at(call: &Call, idx: usize) -> &call::Leg {
    if idx == usize::MAX {
        &call.a_leg
    } else {
        &call.b_legs[idx]
    }
}

/// Project a canonical [`Method`] onto the in-dialog admissibility view the
/// in-dialog generators accept — `None` for methods that may not be sent as an
/// ordinary in-dialog request (ACK/CANCEL, out-of-dialog-only). Formerly a
/// hand-rolled `String → enum` re-parse; now a thin view over `Method`.
fn in_dialog_method(method: &Method) -> Option<InDialogMethod> {
    InDialogMethod::try_from(method).ok()
}

fn top_via_dest(req: &sip_message::SipRequest) -> (String, u16) {
    if let Some(via) = get_header(&req.headers, "via") {
        if let Some(after) = via.split_whitespace().nth(1) {
            if let Some(sent_by) = after.split(';').next() {
                return relay::dest_of(sent_by.trim());
            }
        }
    }
    ("127.0.0.1".to_string(), 5060)
}

fn unwrap_angle(value: &str) -> String {
    let t = value.trim();
    match (t.find('<'), t.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => t[a + 1..b].to_string(),
        _ => t.to_string(),
    }
}

/// Structural headers the response generator owns — never settable via the flat
/// header map (ADR-0017 X2). `Contact` is excluded because a redirect authors it
/// from the typed contact list and a reject carries none.
const A_LEG_RESPONSE_STRUCTURAL: &[&str] = &[
    "from", "to", "via", "call-id", "cseq", "max-forwards", "content-length", "record-route",
    "contact",
];

/// Build the extra a-leg response headers for a decision-authored Reject/Redirect
/// ([`RuleAction::RespondToALeg`]): non-structural `header_updates` *sets* plus one
/// `Contact: <uri>;q=…` per redirect target. Removals and structural keys drop.
fn build_a_leg_response_headers(
    header_updates: &[(String, Option<String>)],
    contacts: &[(String, Option<f32>)],
) -> Vec<sip_message::SipHeader> {
    let mut out: Vec<sip_message::SipHeader> = Vec::new();
    for (name, val) in header_updates {
        let is_structural =
            A_LEG_RESPONSE_STRUCTURAL.contains(&name.to_ascii_lowercase().as_str());
        if let (Some(v), false) = (val, is_structural) {
            out.push(sip_message::SipHeader { name: name.clone(), value: v.clone() });
        }
    }
    for (uri, q) in contacts {
        let value = match q {
            Some(q) => format!("<{uri}>;q={q}"),
            None => format!("<{uri}>"),
        };
        out.push(sip_message::SipHeader { name: "Contact".to_string(), value });
    }
    out
}
