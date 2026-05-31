//! `ActionExecutor` — translates a rule's [`RuleAction`]s into a [`HandlerResult`]
//! (updated [`Call`] + typed [`HandlerEffects`]). Port of `ActionExecutor.ts`,
//! scoped to the basic-B2BUA action set. State mutations use the `call`-crate
//! lens helpers; outbound messages use the [`relay`] primitives.

use call::helpers::{
    add_b_leg, add_cdr_event, add_tag_mapping, deactivate_rule, merge_leg, replace_timer_by_id,
    set_bye_disposition, set_leg_disposition, set_leg_state, split_leg, TERMINATING_TIMEOUT_MS,
};
use call::{
    B2buaDialogExt, ByeDisposition, Call, CdrEvent, Dialog, LegDisposition, LegState, StackDialog,
    TagMapping, TimerEntry, TimerType,
};
use sip_message::generators::{
    self, GenerateInDialogRequestOpts, GenerateResponseOpts, InDialogMethod,
    InviteClientTransactionHandle,
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
                if let Some(peer) = peer_leg_id(call, ctx.source_leg_id) {
                    self.relay_to(call, fx, ctx, &peer, transform);
                }
            }
            RuleAction::RelayToLeg { leg_id, transform } => {
                self.relay_to(call, fx, ctx, leg_id, transform);
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
                        relay::ack_b_leg(&call.call_ref, leg, self.config, self.id_gen)
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
        }
    }

    /// Relay the current event to `target_leg` (response → regenerate on the
    /// a-leg; ACK → ack the b-leg; other in-dialog request → regenerate on the
    /// peer dialog).
    fn relay_to(
        &self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
        target_leg: &str,
        transform: &MessageTransform,
    ) {
        if let Some(resp) = ctx.response() {
            // Response from a b-leg → regenerate on the a-leg server transaction.
            let status = transform.status.unwrap_or(resp.status);
            let reason = transform
                .reason
                .clone()
                .unwrap_or_else(|| resp.reason.clone());
            let a_tag = self.ensure_a_dialog(call);
            let a_invite = relay::rebuild_a_leg_invite(call);
            let contact = relay::leg_contact(self.config, &call.call_ref, &call.a_leg.leg_id);
            let content_type = get_header(&resp.headers, "content-type").map(str::to_string);
            let effect = relay::response_to_a_leg(
                &a_invite,
                status,
                &reason,
                Some(a_tag),
                Some(contact),
                resp.body.clone(),
                content_type,
                None,
            );
            fx.outbound.push(effect);
            return;
        }
        if let Some(req) = ctx.request() {
            if req.method.eq_ignore_ascii_case("ACK") {
                if let Some(leg) = call.b_legs.iter().find(|l| l.leg_id == target_leg) {
                    if let Some(e) = relay::ack_b_leg(&call.call_ref, leg, self.config, self.id_gen)
                    {
                        fx.outbound.push(e);
                    }
                }
                return;
            }
            // Other in-dialog request → regenerate on the target dialog.
            let method = in_dialog_method(&req.method);
            if let (Some(method), Some(idx)) = (method, leg_index(call, target_leg)) {
                let branch = self.id_gen.new_branch();
                let dialog = match leg_at(call, idx).dialogs.first() {
                    Some(d) => relay::to_gen_dialog(&d.sip),
                    None => return,
                };
                let opts = GenerateInDialogRequestOpts {
                    via: Some(relay::leg_via(self.config, &call.call_ref, target_leg, branch)),
                    contact: Some(relay::leg_contact(self.config, &call.call_ref, target_leg)),
                    body: req.body.clone(),
                    content_type: get_header(&req.headers, "content-type").map(str::to_string),
                    ..Default::default()
                };
                let res = generators::generate_in_dialog_request(method, &dialog, &opts);
                let dest = relay::dest_of(&relay::strip_uri(&dialog.remote_target));
                let kind = if method == InDialogMethod::Invite {
                    TxnKind::Invite
                } else {
                    TxnKind::NonInvite
                };
                fx.outbound.push(OutboundSipEffect {
                    body: OutboundBody::Request(res.request),
                    mode: OutboundTxnMode::NewClient(kind),
                    destination: dest,
                    label: format!("{} → {target_leg}", req.method),
                    leg_id: Some(target_leg.to_string()),
                });
            }
        }
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
        self.ensure_a_dialog(call);
        *call = set_leg_state(call.clone(), &call.a_leg.leg_id.clone(), LegState::Confirmed);
    }

    /// Ensure the a-leg has a dialog with a stable B2BUA-minted local tag; return
    /// that tag (the To-tag presented to alice on every a-facing response).
    fn ensure_a_dialog(&self, call: &mut Call) -> String {
        if let Some(d) = call.a_leg.dialogs.first() {
            if !d.sip.local_tag.is_empty() {
                return d.sip.local_tag.clone();
            }
        }
        let tag = self.id_gen.new_tag();
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

    /// Graceful teardown: BYE every confirmed leg (except the event source),
    /// CANCEL every early b-leg, mark the source done, enter `terminating`, arm
    /// the safety timer.
    fn begin_termination(&self, call: &mut Call, fx: &mut HandlerEffects, source_leg_id: &str) {
        // a-leg.
        if call.a_leg.state == LegState::Confirmed && call.a_leg.leg_id != source_leg_id {
            if let Some(e) = self.bye_to_leg_a(call) {
                fx.outbound.push(e);
            }
            let a = call.a_leg.leg_id.clone();
            *call = set_bye_disposition(call.clone(), &a, ByeDisposition::ByeSent);
        } else if call.a_leg.leg_id == source_leg_id {
            let a = call.a_leg.leg_id.clone();
            *call = set_leg_state(call.clone(), &a, LegState::Terminated);
            *call = set_bye_disposition(call.clone(), &a, ByeDisposition::ByeReceived);
        }
        // b-legs.
        let b_ids: Vec<(String, LegState)> =
            call.b_legs.iter().map(|l| (l.leg_id.clone(), l.state)).collect();
        for (id, state) in b_ids {
            if id == source_leg_id {
                *call = set_leg_state(call.clone(), &id, LegState::Terminated);
                *call = set_bye_disposition(call.clone(), &id, ByeDisposition::ByeReceived);
                continue;
            }
            match state {
                LegState::Confirmed => {
                    if let Some(e) = self.bye_to_b_leg(call, &id) {
                        fx.outbound.push(e);
                    }
                    *call = set_bye_disposition(call.clone(), &id, ByeDisposition::ByeSent);
                }
                LegState::Trying | LegState::Early => {
                    if let Some(e) = self.cancel_to_leg(call, &id) {
                        fx.outbound.push(e);
                    }
                    *call = set_bye_disposition(call.clone(), &id, ByeDisposition::ByeSent);
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
        let kind = if m == InDialogMethod::Invite { TxnKind::Invite } else { TxnKind::NonInvite };
        fx.outbound.push(OutboundSipEffect {
            body: OutboundBody::Request(res.request),
            mode: OutboundTxnMode::NewClient(kind),
            destination: dest,
            label: format!("{method} → {leg_id}"),
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
        Some(OutboundSipEffect {
            body: OutboundBody::Request(res.request),
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
