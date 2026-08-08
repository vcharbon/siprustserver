//! The seed (CORE_LAYER, always-active) REFER rules — they run before the
//! transfer slice exists: intercept the first in-dialog REFER on a bridged B
//! leg (202 + slice seed + `/call/refer` consult), reject an attended-transfer
//! REFER (`?Replaces=`, 501) and any a-leg REFER (501).

use call::{Direction, LegState, TransferPhase, TransferState};
use sip_message::header::{HeaderName, ReferTo};

use super::notify::{notify, SUB_STATE_ACTIVE_60};
use super::ok;
use crate::rules::model::{Match, RuleAction, RuleContext, RuleDefinition, CORE_LAYER};

fn core_rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<crate::rules::model::RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition::core(id, CORE_LAYER, overrides, matcher, handle)
}

fn transfer_active(ctx: &RuleContext) -> bool {
    ctx.call.transfer_active()
}

/// Refer-To names a dialog to replace (`?Replaces=`, RFC 3891 §3) → attended
/// transfer. A Refer-To no reader accepts is not an attended transfer.
fn refer_to_has_replaces(ctx: &RuleContext) -> bool {
    ctx.request()
        .and_then(|r| r.header::<ReferTo>())
        .and_then(Result::ok)
        .is_some_and(|refer_to| refer_to.uri().escaped_header("Replaces").is_some())
}

/// Non-structural REFER headers forwarded verbatim to `/call/refer`. The
/// transfer's own payload headers ride as typed request fields, so they are
/// excluded alongside the stack-owned set.
fn extract_sip_headers(req: &sip_message::SipRequest) -> serde_json::Map<String, serde_json::Value> {
    const SKIP: &[HeaderName] = &[
        HeaderName::From,
        HeaderName::To,
        HeaderName::Via,
        HeaderName::Contact,
        HeaderName::ContentType,
        HeaderName::CallId,
        HeaderName::CSeq,
        HeaderName::MaxForwards,
        HeaderName::ContentLength,
        HeaderName::ReferTo,
        HeaderName::ReferredBy,
    ];
    let mut out = serde_json::Map::new();
    for h in req.headers() {
        if SKIP.iter().any(|n| n.matches(&h.name)) {
            continue;
        }
        out.insert(h.name.to_string(), serde_json::Value::String(h.value.to_string()));
    }
    out
}

/// `Call-ID;to-tag=…;from-tag=…` from the referrer (B) leg's perspective.
fn state_dialog_id(ctx: &RuleContext, leg_id: &str) -> String {
    let leg = ctx.source_leg();
    let (call_id, from_tag) = leg
        .map(|l| (l.call_id.clone(), l.from_tag.clone()))
        .unwrap_or_default();
    let to_tag = ctx
        .source_dialog()
        .map(|d| d.sip.remote_tag.clone())
        .unwrap_or_default();
    let _ = leg_id;
    format!("{call_id};to-tag={to_tag};from-tag={from_tag}")
}

/// The seed (CORE_LAYER, alwaysActive-equivalent) REFER rules — they run before
/// the transfer slice exists.
pub fn transfer_seed_rules() -> Vec<RuleDefinition> {
    vec![
        // ── transfer-reject-replaces — REFER (from-b) with Replaces → 501.
        // Overrides `transfer-reject-second-refer` so a Replaces REFER mid-
        // transfer is 501 (attended) rather than 491.
        core_rule(
            "transfer-reject-replaces",
            &["transfer-reject-second-refer"],
            Match::request().method("REFER").direction(Direction::FromB).filter(refer_to_has_replaces),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 501,
                    reason: "Not Implemented".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
        // ── transfer-reject-a-leg-refer — REFER from the A leg → 501.
        core_rule(
            "transfer-reject-a-leg-refer",
            &[],
            Match::request().method("REFER").direction(Direction::FromA),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 501,
                    reason: "Not Implemented".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
        // ── transfer-intercept-refer — first in-dialog REFER on a bridged B leg.
        core_rule(
            "transfer-intercept-refer",
            &[],
            Match::request()
                .method("REFER")
                .direction(Direction::FromB)
                .leg_states(&[LegState::Confirmed])
                .leg_disposition(call::LegDisposition::Bridged)
                .filter(|ctx| !refer_to_has_replaces(ctx) && !transfer_active(ctx)),
            |ctx| {
                let req = ctx.request()?;
                let leg_id = ctx.source_leg_id.to_string();
                let refer_to =
                    req.raw(HeaderName::ReferTo).next().unwrap_or_default().to_string();
                let referred_by = req.raw(HeaderName::ReferredBy).next().map(str::to_string);

                // Seed the transfer slice (phase refer-authorizing).
                let seed = TransferState {
                    phase: TransferPhase::ReferAuthorizing,
                    referrer_leg_id: leg_id.clone(),
                    refer_to_uri: refer_to.clone(),
                    effective_refer_to_uri: None,
                    callback_context: None,
                    c_leg_id: None,
                    refer_cseq: Some(req.cseq().seq()),
                    started_at_ms: ctx.now_ms,
                    last_c_leg_notified_status: None,
                    c_initial_sdp: None,
                };

                // Build the /call/refer request JSON the interpreter reposts.
                let dialog_id = state_dialog_id(ctx, &leg_id);
                let mut request = serde_json::Map::new();
                request.insert("call_id".into(), serde_json::json!(ctx.call.a_leg().call_id));
                request.insert("dialog_id".into(), serde_json::json!(dialog_id));
                request.insert("refer_to".into(), serde_json::json!(refer_to));
                if let Some(rb) = &referred_by {
                    request.insert("referred_by".into(), serde_json::json!(rb));
                }
                request.insert("sip_headers".into(), serde_json::Value::Object(extract_sip_headers(req)));

                ok(vec![
                    RuleAction::Respond {
                        status: 202,
                        reason: "Accepted".to_string(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::SetTransfer { state: Some(seed) },
                    RuleAction::ScheduleTimer {
                        timer_type: call::TimerType::ReferSubscriptionExpiry,
                        delay_sec: ctx.config.refer_subscription_expiry_sec,
                        leg_id: None,
                    },
                    RuleAction::ScheduleTimer {
                        timer_type: call::TimerType::ReferOverallSafety,
                        delay_sec: ctx.config.refer_overall_safety_sec,
                        leg_id: None,
                    },
                    notify(&leg_id, SUB_STATE_ACTIVE_60, 100, "Trying"),
                    RuleAction::ReferAsyncHttp {
                        request: serde_json::Value::Object(request),
                    },
                ])
            },
        ),
    ]
}
