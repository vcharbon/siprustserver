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
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
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
    pub fn execute(&self, actions: &[RuleAction], ctx: &RuleContext) -> HandlerResult {
        let mut call = ctx.call.clone();
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
                if let Some(leg) = call.b_legs.iter().find(|l| &l.leg_id == leg_id) {
                    if let Some(e) =
                        relay::ack_b_leg(&call.call_ref, leg, self.config, self.id_gen, vec![], None)
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
                no_answer_timeout_sec,
                callback_context,
            } => {
                let n = call.b_legs.len() + 1;
                let leg_id = format!("b-{n}");
                let a_invite = relay::rebuild_a_leg_invite(call);
                let (leg, effect) = relay::build_b_leg(
                    &call.call_ref,
                    &leg_id,
                    &a_invite,
                    destination.clone(),
                    new_ruri.as_deref(),
                    *no_answer_timeout_sec,
                    self.config,
                    self.id_gen,
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
            RuleAction::ScheduleTimer {
                timer_type,
                delay_sec,
                leg_id,
            } => {
                self.schedule(call, fx, *timer_type, delay_sec * 1000, leg_id.clone());
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
            RuleAction::BeginTermination { .. } => {
                self.begin_termination(call, fx, ctx.source_leg_id);
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
            RuleAction::SendRequestToLeg { leg_id, method } => {
                self.send_request_to_leg(call, fx, leg_id, method);
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
                };
                let (peer, target_to_tag) = resolve_peer(call, ctx);
                if let Some(peer) = peer {
                    self.relay_to(call, fx, ctx, &peer, &transform, target_to_tag);
                }
            }
        }
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
        if req.method.eq_ignore_ascii_case("ACK") {
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
            via: Some(relay::leg_via(self.config, &call.call_ref, target_leg, branch.clone())),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, target_leg)),
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
                method: req.method.to_uppercase(),
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
        // Require/RSeq on a bare-180 downgrade).
        let filter_passthrough = |hs: Vec<sip_message::SipHeader>| -> Vec<sip_message::SipHeader> {
            if transform.remove_headers.is_empty() {
                hs
            } else {
                hs.into_iter()
                    .filter(|h| {
                        !transform
                            .remove_headers
                            .iter()
                            .any(|r| r.eq_ignore_ascii_case(&h.name))
                    })
                    .collect()
            }
        };
        let cseq_num = resp.cseq.seq as i64;
        let cseq_method = if resp.cseq.method.is_empty() {
            "INVITE".to_string()
        } else {
            resp.cseq.method.to_uppercase()
        };
        let to_tag = resp.to.tag.clone().unwrap_or_default();
        let source_leg_id = ctx.source_leg_id.to_string();

        // ── Pending transparent-relay correlation (§8.1.3.3) ──
        if let Some(src_dialog) = ctx.source_dialog().cloned() {
            if let Some(pending) = find_pending_request(&src_dialog, cseq_num).cloned() {
                let contact = relay::leg_contact(self.config, &call.call_ref, target_leg);
                let opts = GenerateRelayedResponseOpts {
                    vias: pending.source_vias.clone(),
                    record_routes: vec![],
                    from: pending.source_from.clone(),
                    to: pending.source_to.clone(),
                    call_id: pending.source_call_id.clone(),
                    cseq: format!("{} {}", pending.inbound_cseq, cseq_method),
                    body: relay_body.clone(),
                    transparent_headers: filter_passthrough(relay::relay_response_passthrough_headers(resp)),
                    content_type: relay_content_type.clone(),
                    contact: Some(contact),
                };
                let relayed = generators::generate_relayed_response(status, &reason, &opts);
                let s_id = dialog_identity_tag(&source_leg_id, &src_dialog);
                *call = remove_pending_request(call.clone(), &source_leg_id, &s_id, cseq_num);
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

        // ── Reliable-1xx early-dialog tracking (b-leg) + a-facing tag map ──
        // Each callee early dialog (forking → several per b-leg) gets its own
        // a-facing tag so the caller sees independent early dialogs; the 1xx is
        // relayed under that per-fork tag (RFC 3261 §12; source confirm/relay).
        if cseq_method == "INVITE"
            && (100..200).contains(&resp.status)
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
            let a_invite = relay::rebuild_a_leg_invite(call);
            let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id);
            let passthrough = filter_passthrough(relay::relay_response_passthrough_headers(resp));
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

        // ── Default: regenerate the INVITE response on the a-leg server txn ──
        let a_tag = self.ensure_a_dialog(call);
        let a_invite = relay::rebuild_a_leg_invite(call);
        let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id);
        // Reliable-provisional negotiation headers (Require/Supported/RSeq) pass
        // through transparently so end-to-end PRACK keeps working (RFC 3262).
        let passthrough = filter_passthrough(relay::relay_response_passthrough_headers(resp));
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
            } else {
                // Additional fork: append a fresh independent early dialog
                // (RFC 3261 §12.1.2). Its INVITE handle falls back to the leg's.
                let ctx = call::helpers::MakeDialogLegCtx {
                    call_id: &leg.call_id,
                    local_uri: leg.local_uri.as_deref().unwrap_or(""),
                    remote_uri: leg.remote_uri.as_deref().unwrap_or(""),
                    local_tag: &leg.from_tag,
                    remote_tag: to_tag,
                };
                let mut d = call::helpers::make_empty_dialog(&ctx, invite_cseq);
                if !contact.is_empty() {
                    d.sip.remote_target = contact.clone();
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
        if let Some(leg) = call.b_legs.iter_mut().find(|l| l.leg_id == leg_id) {
            if let Some(d) = leg.dialogs.first_mut() {
                if !remote_tag.is_empty() {
                    d.sip.remote_tag = remote_tag;
                }
                if !remote_target.is_empty() {
                    d.sip.remote_target = remote_target;
                }
                d.ext.remote_cseq = Some(resp.cseq.seq as i64);
            }
            leg.state = LegState::Confirmed;
            leg.disposition = LegDisposition::Bridged;
        }
        // Reuse the a-facing tag pre-seeded for this callee (relayFirst18x's
        // `force-tag-consistency`) so the 200 OK To-tag matches the first 180.
        let preferred = find_by_b_tag(call, leg_id, &remote_tag_clone).map(|m| m.a_tag.clone());
        self.ensure_a_dialog_with(call, preferred);
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
        let a_invite = relay::rebuild_a_leg_invite(call);
        let remote_target = get_header(&a_invite.headers, "contact")
            .map(unwrap_angle)
            .unwrap_or_else(|| a_invite.from.uri.clone());
        let route_set: Vec<String> = sip_message::message_helpers::get_headers(&a_invite.headers, "record-route")
            .iter()
            .map(|s| s.to_string())
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
        let id = match &leg_id {
            Some(l) => format!("{:?}:{}", timer_type, l),
            None => format!("{:?}", timer_type),
        };
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
    fn begin_termination(&self, call: &mut Call, fx: &mut HandlerEffects, _source_leg_id: &str) {
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
                    let e = if is_a { self.bye_to_leg_a(call) } else { self.bye_to_b_leg(call, &id) };
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
                if let Some(e) = self.bye_to_b_leg(call, leg_id) {
                    fx.outbound.push(e);
                }
                *call = set_bye_disposition(call.clone(), leg_id, ByeDisposition::ByeSent);
            }
            Some(LegState::Trying) | Some(LegState::Early) => {
                if let Some(e) = self.cancel_to_leg(call, leg_id) {
                    fx.outbound.push(e);
                }
                *call = set_bye_disposition(call.clone(), leg_id, ByeDisposition::Cancelled);
            }
            _ => {}
        }
        *call = set_leg_state(call.clone(), leg_id, LegState::Terminated);
    }

    fn send_request_to_leg(&self, call: &mut Call, fx: &mut HandlerEffects, leg_id: &str, method: &str) {
        let m = match in_dialog_method(method) {
            Some(m) => m,
            None => return,
        };
        let idx = match leg_index(call, leg_id) {
            Some(i) => i,
            None => return,
        };
        let dialog = match leg_at(call, idx).dialogs.first() {
            Some(d) => relay::to_gen_dialog(&d.sip),
            None => return,
        };
        let branch = self.id_gen.new_branch();
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id)),
            ..Default::default()
        };
        let res = generators::generate_in_dialog_request(m, &dialog, &opts);
        let dest = relay::dest_of(&relay::strip_uri(&dialog.remote_target));
        let (out_req, dest) =
            relay::apply_b_leg_egress(self.config, leg_id, &dialog.route_set, res.request, dest);
        let kind = if m == InDialogMethod::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(out_req),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("{method} → {leg_id}"),
            leg_id: Some(leg_id.to_string()),
        });
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
        let t_id = dialog_identity_tag(leg_id, &dialog);
        let outbound_cseq = dialog.sip.local_cseq + 1;
        *call = bump_local_cseq(call.clone(), leg_id, &t_id, 1);

        let branch = self.id_gen.new_branch();
        let gen_dialog = relay::to_gen_dialog(&dialog.sip);
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, &call.call_ref, leg_id, branch)),
            contact: Some(relay::leg_contact(self.config, &call.call_ref, leg_id)),
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

    fn bye_to_b_leg(&self, call: &Call, leg_id: &str) -> Option<OutboundSipEffect> {
        let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
        let d = leg.dialogs.first()?;
        self.bye_on_dialog(&call.call_ref, leg_id, &d.sip)
    }

    fn bye_to_leg_a(&self, call: &Call) -> Option<OutboundSipEffect> {
        let d = call.a_leg.dialogs.first()?;
        self.bye_on_dialog(&call.call_ref, &call.a_leg.leg_id, &d.sip)
    }

    fn bye_on_dialog(&self, call_ref: &str, leg_id: &str, sip: &StackDialog) -> Option<OutboundSipEffect> {
        if sip.remote_tag.is_empty() {
            return None; // not a confirmed dialog
        }
        let dialog = relay::to_gen_dialog(sip);
        let branch = self.id_gen.new_branch();
        let opts = GenerateInDialogRequestOpts {
            via: Some(relay::leg_via(self.config, call_ref, leg_id, branch)),
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

    fn cancel_to_leg(&self, call: &Call, leg_id: &str) -> Option<OutboundSipEffect> {
        let leg = call.b_legs.iter().find(|l| l.leg_id == leg_id)?;
        let d = leg.dialogs.first()?;
        let inv_bytes = &d.ext.pending_invite_txn.as_ref()?.original_invite;
        let parsed = CustomParser::new().parse(inv_bytes).ok()?;
        let req = match parsed {
            SipMessage::Request(r) => r,
            _ => return None,
        };
        let cancel = generators::generate_cancel(&InviteClientTransactionHandle {
            original_invite: req,
        });
        Some(OutboundSipEffect {
            body: OutboundBody::Request(cancel),
            mode: OutboundTxnMode::Raw,
            destination: (leg.source.address.clone(), leg.source.port),
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
/// dialog tag. Port of `executeRelayToPeer`'s fallback: an a-leg in-dialog
/// request (PRACK/UPDATE) before the a↔b merge carries the B2BUA's a-facing tag
/// in its To, which the tag map resolves to the right b-leg + callee fork tag.
fn resolve_peer(call: &Call, ctx: &RuleContext) -> (Option<String>, Option<String>) {
    if ctx.source_leg_id == call.a_leg.leg_id {
        if let Some(req) = ctx.request() {
            if let Some(tag) = req.to.tag.as_deref() {
                if let Some(m) = call::helpers::find_by_a_tag(call, tag) {
                    return (Some(m.b_leg_id.clone()), Some(m.b_tag.clone()));
                }
            }
        }
    }
    (peer_leg_id(call, ctx.source_leg_id), None)
}

fn peer_leg_id(call: &Call, leg_id: &str) -> Option<String> {
    if let Some(p) = &call.active_peer {
        if p.leg_a == leg_id {
            return Some(p.leg_b.clone());
        }
        if p.leg_b == leg_id {
            return Some(p.leg_a.clone());
        }
    }
    if leg_id == call.a_leg.leg_id {
        return call.b_legs.iter().find(|l| l.state == LegState::Confirmed).map(|l| l.leg_id.clone())
            .or_else(|| call.b_legs.first().map(|l| l.leg_id.clone()));
    }
    Some(call.a_leg.leg_id.clone())
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

fn in_dialog_method(method: &str) -> Option<InDialogMethod> {
    Some(match method.to_ascii_uppercase().as_str() {
        "BYE" => InDialogMethod::Bye,
        "INVITE" => InDialogMethod::Invite,
        "PRACK" => InDialogMethod::Prack,
        "NOTIFY" => InDialogMethod::Notify,
        "OPTIONS" => InDialogMethod::Options,
        "INFO" => InDialogMethod::Info,
        "UPDATE" => InDialogMethod::Update,
        "MESSAGE" => InDialogMethod::Message,
        _ => return None,
    })
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
