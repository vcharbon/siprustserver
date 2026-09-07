//! The seed (CORE_LAYER) REFER rules — they run before the transfer slice
//! exists: intercept the first in-dialog REFER on a bridged B leg (202 + slice
//! seed + `/call/refer` consult), refuse an unreadable Refer-To (400), reject
//! an attended-transfer REFER (`?Replaces=`, 501) and any a-leg REFER (501).
//!
//! Every one of them is gated on the decision layer's LOCAL-processing
//! directive (`features.refer`): without it this platform terminates no REFER
//! at all and the CORE `relay-refer` forwards it to the peer leg like any other
//! in-dialog method, Refer-To or not (RFC 3515 rides end to end).

use call::{Direction, LegState, TransferPhase, TransferState};
use sip_message::header::{HeaderName, ReferTo};

use super::notify::{notify, SUB_STATE_ACTIVE_60};
use super::ok;
use crate::rules::model::{Match, RuleAction, RuleContext, RuleDefinition, TimerDelay, CORE_LAYER};

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

/// The routing decision directed LOCAL REFER processing for this call
/// (`features.refer`) — the precondition of every rule in this module.
fn local_refer(ctx: &RuleContext) -> bool {
    ctx.call.refer_processed_locally()
}

/// The REFER carries a Refer-To this stack can read: present, and a name-addr /
/// addr-spec whose URI parses (RFC 3515 §2, RFC 3261 §20.30 / §25.1). The
/// judgement is `sip-message`'s — `Refer-To` is parsed eagerly and non-fatally
/// at intake, so an unreadable one reaches here as a parse error.
fn refer_to_readable(ctx: &RuleContext) -> bool {
    ctx.request().and_then(|r| r.header::<ReferTo>()).is_some_and(|h| h.is_ok())
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

/// The seed (CORE_LAYER) REFER rules — they run before the transfer slice
/// exists, and only on a call whose route activated local REFER processing.
pub fn transfer_seed_rules() -> Vec<RuleDefinition> {
    vec![
        // ── transfer-reject-replaces — REFER (from-b) with Replaces → 501.
        // Overrides `transfer-reject-second-refer` so a Replaces REFER mid-
        // transfer is 501 (attended) rather than 491.
        core_rule(
            "transfer-reject-replaces",
            &["transfer-reject-second-refer"],
            Match::request()
                .method("REFER")
                .direction(Direction::FromB)
                .filter(|ctx| local_refer(ctx) && refer_to_has_replaces(ctx)),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 501,
                    reason: "Not Implemented".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
        // ── transfer-reject-a-leg-refer — REFER from the A leg → 501. Local
        // processing is the b-leg transferor's; the a-leg has no transfer here.
        core_rule(
            "transfer-reject-a-leg-refer",
            &[],
            Match::request().method("REFER").direction(Direction::FromA).filter(local_refer),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 501,
                    reason: "Not Implemented".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
        // ── transfer-refuse-unreadable-refer-to — a REFER this platform is to
        // process itself, whose Refer-To is absent or unreadable → 400. The
        // transfer target is the request's whole point (RFC 3515 §2), so there
        // is nothing to authorize and nothing to accept: refusing the request
        // is the answer, never a 202 for a transfer that cannot start.
        core_rule(
            "transfer-refuse-unreadable-refer-to",
            &[],
            Match::request()
                .method("REFER")
                .direction(Direction::FromB)
                .leg_states(&[LegState::Confirmed])
                .leg_disposition(call::LegDisposition::Bridged)
                .filter(|ctx| local_refer(ctx) && !refer_to_readable(ctx) && !transfer_active(ctx)),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 400,
                    reason: "Bad Request".to_string(),
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
                .filter(|ctx| {
                    local_refer(ctx)
                        && refer_to_readable(ctx)
                        && !refer_to_has_replaces(ctx)
                        && !transfer_active(ctx)
                }),
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
                    subscription_terminated: false,
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

                let first_notify = notify(&seed, SUB_STATE_ACTIVE_60, 100, "Trying");
                let mut actions = vec![
                    RuleAction::Respond {
                        status: 202,
                        reason: "Accepted".to_string(),
                        body: vec![],
                        content_type: None,
                    },
                    RuleAction::SetTransfer { state: Some(seed) },
                    RuleAction::ScheduleTimer {
                        timer_type: call::TimerType::ReferSubscriptionExpiry,
                        delay: TimerDelay::secs(ctx.config.refer_subscription_expiry_sec),
                        leg_id: None,
                    },
                    RuleAction::ScheduleTimer {
                        timer_type: call::TimerType::ReferOverallSafety,
                        delay: TimerDelay::secs(ctx.config.refer_overall_safety_sec),
                        leg_id: None,
                    },
                ];
                actions.extend(first_notify);
                actions.push(RuleAction::ReferAsyncHttp {
                    request: serde_json::Value::Object(request),
                });
                ok(actions)
            },
        ),
    ]
}
