//! The non-INVITE request records (ADR-0055 §2): where each record is
//! written as its request leaves or is held, and what each way it settles
//! owes. A leg's end and the call's release settle through the one settle
//! entry ([`super::settle`]).
//!
//! | settled by | relay | own | held |
//! |---|---|---|---|
//! | its final | the final is relayed to the originator | nothing more | — |
//! | its timeout (RFC 3261 §17.1.2.2) | nothing: no 408 to a non-INVITE (RFC 4320 §4.2) | the method table's own timeout, read by `handle-timeout` | — |
//! | its leg's end | the method table's owed final (RFC 3261 §8.2.6) | stays until its final or timeout | 487 (§15.1.2) |
//! | the call's release | as its leg's end | as its leg's end | 487 |
//! | its deadline | — | — | 500 (RFC 4320 §4.2) |
//! | a service's answer | — | — | that answer |
//!
//! A relay's final settles it as it is relayed
//! ([`super::relay_pending_response`]); every other final and every timeout
//! settles its record before the rules read the event, which they read
//! through [`call::helpers::Resolved::record`].

use call::helpers::{
    add_record, close_held, close_record, disarm_timer, find_leg, held_record, next_held_deadline,
    record_with_branch, take_overdue_held, Resolved,
};
use call::{
    Call, DialogId, HostPort, RequestOrigin, RequestRecord, SentRequest, TimerKey, TimerType,
};
use sip_message::generators::{self, GenerateError};
use sip_message::header::{HeaderName, MediaType};
use sip_message::{Method, SipMessage, SipRequest, SipResponse};

use crate::effects::{
    CriticalStateEffect, HandlerEffects, OutboundBody, OutboundSipEffect, OutboundTxnMode,
    Provenance,
};
use crate::rules::method_table::usage_of;
use crate::rules::relay;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::RuleContext;

use super::snapshot_answer::{snapshot_response_opts, stack_answer, via_response_target};
use super::ActionExecutor;

/// The final the platform gives a held request no service answered by its
/// deadline: the service failed to.
const UNANSWERED_HELD: (u16, &str) = (500, "Server Internal Error");

fn held_answer_key() -> TimerKey {
    TimerKey::call(TimerType::HeldAnswer)
}

/// The transaction `req` opened leaving toward `dest`.
pub(super) fn sent_request(req: &SipRequest, dest: &(String, u16)) -> SentRequest {
    SentRequest {
        branch: req.top_via().branch().unwrap_or_default().to_string(),
        image: req.image().to_vec(),
        destination: HostPort { host: dest.0.clone(), port: dest.1 },
    }
}

impl ActionExecutor<'_> {
    /// Record this stack's own non-INVITE `req`, leaving toward `dest` on
    /// `leg_id`'s dialog `id`: it awaits its final until Timer F.
    fn record_own(
        &mut self,
        call: &mut Call,
        leg_id: &str,
        id: &DialogId,
        req: &SipRequest,
        dest: &(String, u16),
    ) {
        let record = RequestRecord {
            relayed_by: self.owners.this.clone(),
            author: self.turn.rule.map(str::to_string),
            ..RequestRecord::own(
                req.method().as_str(),
                i64::from(req.cseq().seq()),
                sent_request(req, dest),
                self.now_ms + RequestRecord::TIMER_F_MS,
                usage_of(req),
            )
        };
        add_record(call, leg_id, id, record);
    }

    /// Record this stack's own non-INVITE request `effect` carries, leaving on
    /// `leg_id`'s dialog `id`.
    pub(super) fn record_own_effect(
        &mut self,
        call: &mut Call,
        leg_id: &str,
        id: &DialogId,
        effect: &OutboundSipEffect,
    ) {
        if let OutboundBody::Request(req) = &effect.body {
            self.record_own(call, leg_id, id, req, &effect.destination);
        }
    }

    /// The sent half of a non-INVITE relay leaving as `out` toward `dest`:
    /// its transaction, its Timer F and the usage it belongs to.
    pub(super) fn relayed_sent_half(
        &self,
        record: RequestRecord,
        out: &SipRequest,
        dest: &(String, u16),
    ) -> RequestRecord {
        RequestRecord {
            sent: Some(sent_request(out, dest)),
            deadline_ms: Some(self.now_ms + RequestRecord::TIMER_F_MS),
            usage: usage_of(out),
            ..record
        }
    }

    /// Settle the record `event` ends before the rules read it, noting it on
    /// `resolved`: this stack's own request answered by a final, or any
    /// request whose transaction on `leg_id` timed out. A relay answered by a
    /// final is settled as that final is relayed.
    pub(crate) fn settle_before_rules(
        &mut self,
        call: &mut Call,
        event: &CallEvent,
        leg_id: &str,
        resolved: &mut Resolved,
    ) {
        match event {
            CallEvent::Sip { message, .. } => {
                let SipMessage::Response(resp) = message.as_ref() else { return };
                if resp.status() < 200 || resp.cseq().method() == Method::Invite {
                    return;
                }
                let Some((holder, record)) = resolved.record.as_deref() else { return };
                if record.is_own() {
                    close_record(call, leg_id, holder, record.outbound_cseq);
                }
            }
            CallEvent::Timeout { branch, .. } => {
                let Some((holder, record)) = record_with_branch(call, leg_id, branch) else {
                    return;
                };
                close_record(call, leg_id, &holder, record.outbound_cseq);
                resolved.record = Some(Box::new((holder, record)));
            }
            _ => {}
        }
    }

    /// The [`TimerType::HeldAnswer`] fired: every held request whose
    /// deadline passed is answered by the platform.
    pub(crate) fn answer_overdue_held(&mut self, call: &mut Call, fx: &mut HandlerEffects) {
        let overdue = take_overdue_held(call, self.now_ms);
        for (leg_id, _, record) in overdue {
            let (status, reason) = UNANSWERED_HELD;
            answer_received(call, fx, &leg_id, &record, status, reason, Vec::new(), None);
        }
        arm_held_answer(call, fx, self.now_ms);
    }

    /// Hold the turn's received request on the dialog it names for a final a
    /// service gives later ([`b2bua_sdk::model::RuleAction::HoldRequest`]).
    /// A request no record can hold — an INVITE, an ACK, a CANCEL, or one
    /// naming no dialog — is answered `500` at once rather than left to its
    /// sender's Timer F.
    pub(super) fn hold_request(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        ctx: &RuleContext,
    ) {
        let Some(req) = ctx.request() else { return };
        let holdable = !matches!(req.method(), Method::Invite | Method::Ack | Method::Cancel);
        let Some(id) = ctx.request_dialog_id().filter(|_| holdable).cloned() else {
            let (status, reason) = UNANSWERED_HELD;
            self.respond(call, fx, ctx, status, reason, &[], None, vec![]);
            return;
        };
        let record = RequestRecord {
            origin: RequestOrigin::Held,
            usage: usage_of(req),
            deadline_ms: Some(self.now_ms + RequestRecord::HELD_ANSWER_MS),
            source_vias: req.raw(HeaderName::Via).map(str::to_string).collect(),
            source_call_id: req.call_id().as_str().to_string(),
            source_from: req.raw(HeaderName::From).next().unwrap_or_default().to_string(),
            source_to: req.raw(HeaderName::To).next().unwrap_or_default().to_string(),
            source_timestamp: req.raw(HeaderName::Timestamp).next().map(str::to_string),
            relayed_by: self.owners.this.clone(),
            ..RequestRecord::relayed(req.method().as_str(), 0, i64::from(req.cseq().seq()))
        };
        add_record(call, ctx.source_leg_id, &id, record);
        arm_held_answer(call, fx, self.now_ms);
    }

    /// Answer the request held on `leg_id` numbered `cseq` with `status`
    /// ([`b2bua_sdk::model::RuleAction::AnswerHeld`]). One no longer held
    /// draws nothing.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn answer_held(
        &mut self,
        call: &mut Call,
        fx: &mut HandlerEffects,
        leg_id: &str,
        cseq: i64,
        status: u16,
        reason: &str,
        body: &[u8],
        content_type: Option<&str>,
    ) {
        let held = find_leg(call, leg_id).and_then(|leg| {
            leg.dialogs.iter().find_map(|d| {
                d.requests.iter().find(|r| r.is_held() && r.inbound_cseq == cseq).cloned()
            })
        });
        let Some(record) = held else { return };
        let Some((holder, _)) = held_record(call, leg_id, &record.method, cseq) else { return };
        close_held(call, leg_id, &holder, cseq);
        let content_type = content_type.and_then(relay::media_type);
        answer_received(call, fx, leg_id, &record, status, reason, body.to_vec(), content_type);
        arm_held_answer(call, fx, self.now_ms);
    }
}

/// The [`TimerType::HeldAnswer`] stands at the earliest deadline of a held
/// request, never before `now_ms`, and is off while none is held.
pub(super) fn arm_held_answer(call: &mut Call, fx: &mut HandlerEffects, now_ms: i64) {
    match next_held_deadline(call) {
        Some(at) => crate::timer_arm::arm(call, fx, held_answer_key(), at.max(now_ms)),
        None => {
            if let Some(entry) = disarm_timer(call, &held_answer_key()) {
                fx.critical.push(CriticalStateEffect::CancelTimer { key: entry.key });
            }
        }
    }
}

/// The final `status` to the received half of `record`, sent on the leg
/// `originator` to its top Via response target (RFC 3261 §18.2.2): the leg a
/// held request arrived on, or the one a relay came from.
#[allow(clippy::too_many_arguments)]
pub(super) fn answer_received(
    call: &Call,
    fx: &mut HandlerEffects,
    originator: &str,
    record: &RequestRecord,
    status: u16,
    reason: &str,
    body: Vec<u8>,
    content_type: Option<MediaType>,
) {
    let Some(dest) = record.source_vias.first().and_then(|v| via_response_target(v)) else {
        tracing::warn!(
            call_ref = %call.call_ref,
            leg_id = %originator,
            method = %record.method,
            "request left unanswered — the originator's top Via names no destination"
        );
        return;
    };
    let Ok(answer) = received_answer(record, &record.method, status, reason, body, content_type)
    else {
        tracing::warn!(
            call_ref = %call.call_ref,
            leg_id = %originator,
            method = %record.method,
            "request left unanswered — the originator's request lines make no response"
        );
        return;
    };
    fx.outbound.push(OutboundSipEffect {
        body: OutboundBody::Response(answer),
        mode: OutboundTxnMode::ServerResponse,
        destination: dest,
        label: format!("{status} {} → {originator}", record.method),
        leg_id: Some(originator.to_string()),
        provenance: Provenance::Authored,
    });
}

/// The final `status` to `record`'s received half, carrying `body`.
fn received_answer(
    record: &RequestRecord,
    method: &str,
    status: u16,
    reason: &str,
    body: Vec<u8>,
    content_type: Option<MediaType>,
) -> Result<SipResponse, GenerateError> {
    if body.is_empty() {
        return stack_answer(record, method, status, reason);
    }
    let opts = snapshot_response_opts(record, method, body, content_type, Vec::new(), None);
    generators::generate_relayed_response(status, reason, &opts)
}
