//! ReferAuthorizing phase: resolution of the `/call/refer` consult — denial,
//! authorization (dial C on held SDP), and the consult-hung timeout.

use b2bua_sdk::sm_rule;
use call::TransferPhase;
use sip_message::header::{HeaderValue, ReferTo};
use sip_message::{Method, SipStr};

use super::{state, timer_id, Phase, TRANSFER_MACHINE};
use crate::rules::defaults::parse_header_updates;
use crate::rules::model::{
    Effect, Match, RuleAction, RuleContext, RuleDefinition, RuleDiagnostic, RuleHandleResult,
};
use crate::rules::refer_transfer::notify::{
    notify, SUB_STATE_TERMINATED_NORESOURCE, SUB_STATE_TERMINATED_TIMEOUT,
};
use crate::rules::refer_transfer::ok;
use crate::rules::{relay, Terminal};

/// Reduce a Refer-To to the bare `sip:user@host:port;params` URI a
/// Request-URI may carry: the display name and the `?headers` list drop
/// (RFC 3261 §19.1.1 forbids escaped headers in a Request-URI).
///
/// Errs when the value does not read. The transfer target is what the C leg
/// dials, so keeping unreadable text verbatim would originate toward an address
/// nobody named — the referrer is told the transfer failed instead.
fn to_bare_uri(refer_to: &str) -> Result<String, relay::UnreadableAddress> {
    ReferTo::parse(&SipStr::owned(refer_to))
        .map(|value| value.uri().clone().without_escaped_headers().to_string())
        .map_err(|err| relay::UnreadableAddress {
            field: "refer_to",
            value: refer_to.to_string(),
            reason: err.reason,
        })
}

/// End a transfer whose authorization named a target no reader accepts: tell the
/// referrer the transfer failed (NOTIFY `terminated`, 502), disarm the transfer
/// watchdogs and clear the slice — the same terminal shape a `/call/refer` denial
/// takes. The call itself survives; only the transfer is abandoned.
fn refuse_transfer(ctx: &RuleContext, field: &str, reason: &str) -> Option<RuleHandleResult> {
    let st = state(ctx)?;
    let mut actions = Vec::new();
    actions.extend(notify(
        st,
        SUB_STATE_TERMINATED_NORESOURCE,
        502,
        &format!("Unreadable Transfer Target ({field})"),
    ));
    actions.extend([
        RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
        RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
        RuleAction::SetTransfer { state: None },
    ]);
    Some(RuleHandleResult::new(actions).with_diagnostic(RuleDiagnostic::unreadable(field, reason)))
}

/// Read the `reject`/`error` reject code+reason from the internal-event payload.
fn reject_code_reason(ctx: &RuleContext) -> (u16, String) {
    let (is_reject, payload) = match ctx.event {
        crate::event::CallEvent::InternalEvent { outcome, payload, .. } => {
            (outcome == "reject", payload)
        }
        _ => (false, &serde_json::Value::Null),
    };
    if is_reject {
        let code =
            payload.get("reject_code").and_then(|v| v.as_u64()).map(|c| c as u16).unwrap_or(603);
        let reason =
            payload.get("reject_reason").and_then(|v| v.as_str()).unwrap_or("Declined").to_string();
        (code, reason)
    } else {
        (500, "Server Internal Error".to_string())
    }
}

/// transfer-http-reject / -error — `/call/refer` denied.
pub(super) fn http_reject() -> RuleDefinition {
    sm_rule! {
        id: "transfer-http-reject",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ReferAuthorizing ],
        transitions: [ Phase::ReferAuthorizing => Terminal ],
        effects: [
            Effect::Originate { method: Method::Notify, label: "NOTIFY terminated → referrer" },
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
        ],
        matcher: Match::internal_event()
            .topic("refer-http-result")
            .filter(|ctx| {
                matches!(
                    ctx.event,
                    crate::event::CallEvent::InternalEvent { outcome, .. }
                        if outcome == "reject" || outcome == "error"
                )
            }),
        handle: |ctx| {
            let st = state(ctx)?;
            let (code, reason) = reject_code_reason(ctx);
            let mut actions = Vec::new();
            actions.extend(notify(st, SUB_STATE_TERMINATED_NORESOURCE, code, &reason));
            actions.extend([
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                RuleAction::SetTransfer { state: None },
            ]);
            ok(actions)
        },
    }
}

/// transfer-http-allow — `/call/refer` authorized → create C leg. An
/// authorization naming a target no reader accepts (unreadable Refer-To,
/// out-of-range port) takes the same terminal path as a denial rather than
/// dialing a fabricated address — hence the second transition edge and the
/// NOTIFY/guard-timer effects.
pub(super) fn http_allow() -> RuleDefinition {
    sm_rule! {
        id: "transfer-http-allow",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ReferAuthorizing ],
        transitions: [
            Phase::ReferAuthorizing => Phase::CRinging,
            Phase::ReferAuthorizing => Terminal,
        ],
        effects: [
            Effect::Originate { method: Method::Invite, label: "INVITE → C (transfer target)" },
            Effect::Originate { method: Method::Notify, label: "NOTIFY terminated → referrer (unreadable target)" },
            Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
        ],
        matcher: Match::internal_event()
            .topic("refer-http-result")
            .outcome("allow"),
        handle: |ctx| {
            let st = state(ctx)?.clone();
            let payload = match ctx.event {
                crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                _ => return None,
            };
            let host = payload.get("destination").and_then(|d| d.get("host")).and_then(|v| v.as_str())?.to_string();
            let port = match crate::decision::read_stated_port(
                payload.get("destination").and_then(|d| d.get("port")),
            ) {
                Some(p) => p,
                None => {
                    return refuse_transfer(
                        ctx,
                        "destination.port",
                        "not a port number in 1..=65535",
                    )
                }
            };
            let no_answer = payload.get("no_answer_timeout_sec").and_then(|v| v.as_i64());
            let callback_context = payload.get("callback_context").and_then(|v| v.as_str()).map(str::to_string);
            let new_refer_to = payload.get("new_refer_to").and_then(|v| v.as_str()).map(str::to_string);
            let header_updates = parse_header_updates(payload);

            // Held SDP from A's INVITE snapshot (preserves codecs, port 0,
            // a=inactive). No profile → drop the body.
            let a_invite = relay::rebuild_a_leg_invite(ctx.call.a_leg_invite());
            let held = a_invite.sdp().and_then(sip_message::extract_codec_profile).map(|profile| {
                sip_message::build_held_sdp_from_profile(
                    &profile,
                    &sip_message::BuildHeldSdpOptions {
                        local_ip: ctx.config.sip_local_ip.clone(),
                        now_ms: ctx.now_ms,
                    },
                )
            });
            // `Some(bytes)` set / `Some(empty)` drop. The C INVITE never
            // carries A's real SDP until the c-realign re-INVITE.
            let body_override = Some(held.unwrap_or_default());

            let raw_refer_to = new_refer_to.unwrap_or_else(|| st.refer_to_uri.clone());
            let effective = match to_bare_uri(&raw_refer_to) {
                Ok(uri) => uri,
                Err(err) => return refuse_transfer(ctx, err.field, &err.reason),
            };
            let c_leg_id = format!("b-{}", ctx.call.b_legs().len() + 1);

            let mut new_state = st.clone();
            new_state.phase = TransferPhase::CRinging;
            new_state.c_leg_id = Some(c_leg_id.clone());
            new_state.effective_refer_to_uri = Some(effective.clone());
            if callback_context.is_some() {
                new_state.callback_context = callback_context.clone();
            }

            ok(vec![
                RuleAction::CreateLeg {
                    destination: (host, port),
                    new_ruri: Some(effective),
                    new_from: None,
                    new_to: None,
                    no_answer_timeout_sec: no_answer,
                    callback_context,
                    body_override,
                    header_updates,
                    kind: None,
                },
                RuleAction::SetTransfer { state: Some(new_state) },
            ])
        },
    }
}

/// transfer-http-timeout — subscription-expiry fired (HTTP hung).
pub(super) fn http_timeout() -> RuleDefinition {
    sm_rule! {
        id: "transfer-http-timeout",
        machine: TRANSFER_MACHINE,
        active: [ Phase::ReferAuthorizing ],
        transitions: [ Phase::ReferAuthorizing => Terminal ],
        effects: [
            Effect::Originate { method: Method::Notify, label: "NOTIFY terminated;timeout → referrer" },
            Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
        ],
        matcher: Match::timer()
            .timer_type(call::TimerType::ReferSubscriptionExpiry),
        handle: |ctx| {
            let st = state(ctx)?;
            let mut actions = Vec::new();
            actions.extend(notify(st, SUB_STATE_TERMINATED_TIMEOUT, 500, "Server Internal Error"));
            actions.extend([
                RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                RuleAction::SetTransfer { state: None },
            ]);
            ok(actions)
        },
    }
}
