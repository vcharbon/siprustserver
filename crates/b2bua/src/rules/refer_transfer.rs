//! `referTransfer` — the REFER-driven blind-transfer service. Port of
//! `src/b2bua/rules/custom/referTransfer.ts` + the seed rules in
//! `src/b2bua/rules/defaults/TransferRules.ts`.
//!
//! There is no `defineService`/typed-ext machinery in the Rust port: the per-call
//! state lives on the typed `Call.transfer` slice (mirrors `promote_pem`), and
//! the rules are stateless `fn`s that read `ctx.call.transfer` and emit
//! `SetTransfer` to advance the phase. Activation guard = "the `transfer` slice
//! is `Some`", checked per rule exactly like `promote_pem_active`.
//!
//! The seed rules (`transfer-intercept-refer`, `transfer-reject-replaces`,
//! `transfer-reject-a-leg-refer`) must fire BEFORE the slice exists, so they are
//! CORE_LAYER `alwaysActive`-equivalent rules gated by their match columns + a
//! `no_transfer_active` filter. The phase-gated service rules are SERVICE_LAYER.
//!
//! Slice 5a ports the refer-authorizing → c-ringing → (initial C 200/fail/
//! no-answer) portion. The c-realign / a-realign / merge rules (5b–5d) and the
//! glare/gating rules (5e) are NOT yet implemented.

use call::{
    CdrEventType, Direction, LegState, TransferPhase, TransferState,
};
use sip_message::message_helpers::get_header;
use sip_message::sipfrag::sipfrag_from_status;

use super::model::{
    Match, RuleAction, RuleContext, RuleDefinition, CORE_LAYER, SERVICE_LAYER,
};

// Subscription-State fragments (RFC 3265 §3.2.4).
const SUB_STATE_ACTIVE_60: &str = "active;expires=60";
const SUB_STATE_TERMINATED_NORESOURCE: &str = "terminated;reason=noresource";
const SUB_STATE_TERMINATED_TIMEOUT: &str = "terminated;reason=timeout";
const SIPFRAG_CT: &str = "message/sipfrag;version=2.0";

fn ok(actions: Vec<RuleAction>) -> Option<super::model::RuleHandleResult> {
    Some(super::model::RuleHandleResult::new(actions))
}

fn core_rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<super::model::RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition { id, layer: CORE_LAYER, overrides, matcher, handle }
}

fn svc_rule(
    id: &'static str,
    overrides: &'static [&'static str],
    matcher: Match,
    handle: fn(&RuleContext) -> Option<super::model::RuleHandleResult>,
) -> RuleDefinition {
    RuleDefinition { id, layer: SERVICE_LAYER, overrides, matcher, handle }
}

// ── Timer-id minting (must match `ActionExecutor::schedule`) ─────────────────
// `schedule` builds `format!("{:?}", t)` (no leg) / `format!("{:?}:{}", t, leg)`.
// Mint cancel ids from the same recipe so they can never drift (CLAUDE.md timer-
// aliasing hazard).
fn timer_id(t: call::TimerType, leg: Option<&str>) -> String {
    match leg {
        Some(l) => format!("{t:?}:{l}"),
        None => format!("{t:?}"),
    }
}

fn transfer_active(ctx: &RuleContext) -> bool {
    call::helpers::transfer_active(ctx.call)
}

fn phase(ctx: &RuleContext) -> Option<TransferPhase> {
    call::helpers::transfer_phase(ctx.call)
}

fn state<'a>(ctx: &'a RuleContext<'a>) -> Option<&'a TransferState> {
    call::helpers::transfer_state(ctx.call)
}

/// Refer-To URI carries a `Replaces=` parameter → attended transfer (RFC 3891).
fn refer_to_has_replaces(ctx: &RuleContext) -> bool {
    match ctx.request().and_then(|r| get_header(&r.headers, "refer-to")) {
        Some(v) => v.to_ascii_lowercase().contains("replaces"),
        None => false,
    }
}

/// Reduce a Refer-To header (`<sip:user@host:port;params>?headers` / display
/// name) to its bare `sip:user@host:port` URI (port of TS `toBareUri`).
fn to_bare_uri(refer_to: &str) -> String {
    let inner = match (refer_to.find('<'), refer_to.find('>')) {
        (Some(a), Some(b)) if b > a + 1 => &refer_to[a + 1..b],
        _ => refer_to.trim(),
    };
    // Drop any embedded URI headers (`?...`) — keep scheme:user@host:port;params.
    inner.split('?').next().unwrap_or(inner).trim().to_string()
}

/// Non-structural REFER headers forwarded verbatim to `/call/refer`
/// (port of TS `extractSipHeaders`).
fn extract_sip_headers(req: &sip_message::SipRequest) -> serde_json::Map<String, serde_json::Value> {
    const SKIP: [&str; 11] = [
        "from", "to", "via", "contact", "content-type", "call-id", "cseq",
        "max-forwards", "content-length", "refer-to", "referred-by",
    ];
    let mut out = serde_json::Map::new();
    for h in &req.headers {
        let name = h.name.to_ascii_lowercase();
        if SKIP.contains(&name.as_str()) {
            continue;
        }
        out.insert(h.name.clone(), serde_json::Value::String(h.value.clone()));
    }
    out
}

/// Read the `reject`/`error` reject code+reason from the internal-event payload
/// (port of `transfer-http-reject`'s mapping, referTransfer.ts:144-152).
fn reject_code_reason(ctx: &RuleContext) -> (u16, String) {
    let (is_reject, payload) = match ctx.event {
        crate::event::CallEvent::InternalEvent { outcome, payload, .. } => {
            (outcome == "reject", payload)
        }
        _ => (false, &serde_json::Value::Null),
    };
    if is_reject {
        let code = payload.get("reject_code").and_then(|v| v.as_u64()).map(|c| c as u16).unwrap_or(603);
        let reason = payload
            .get("reject_reason")
            .and_then(|v| v.as_str())
            .unwrap_or("Declined")
            .to_string();
        (code, reason)
    } else {
        (500, "Server Internal Error".to_string())
    }
}

fn notify(
    leg_id: &str,
    subscription_state: &str,
    code: u16,
    reason: &str,
) -> RuleAction {
    RuleAction::SendNotify {
        leg_id: leg_id.to_string(),
        event: "refer".to_string(),
        subscription_state: subscription_state.to_string(),
        content_type: Some(SIPFRAG_CT.to_string()),
        body: sipfrag_from_status(code, reason),
    }
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
                let refer_to = get_header(&req.headers, "refer-to").unwrap_or_default().to_string();
                let referred_by = get_header(&req.headers, "referred-by").map(str::to_string);

                // Seed the transfer slice (phase refer-authorizing).
                let seed = TransferState {
                    phase: TransferPhase::ReferAuthorizing,
                    referrer_leg_id: leg_id.clone(),
                    refer_to_uri: refer_to.clone(),
                    effective_refer_to_uri: None,
                    callback_context: None,
                    c_leg_id: None,
                    refer_cseq: Some(req.cseq.seq),
                    started_at_ms: ctx.now_ms,
                    last_c_leg_notified_status: None,
                    c_initial_sdp: None,
                };

                // Build the /call/refer request JSON the interpreter reposts.
                let dialog_id = state_dialog_id(ctx, &leg_id);
                let mut request = serde_json::Map::new();
                request.insert("call_id".into(), serde_json::json!(ctx.call.a_leg.call_id));
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

/// The phase-gated SERVICE_LAYER transfer rules. Dormant unless `Call.transfer`
/// is `Some`; SERVICE_LAYER ranks them above the CORE relay rules they displace.
pub fn transfer_rules() -> Vec<RuleDefinition> {
    vec![
        // ── transfer-reject-second-refer — a second REFER while active → 491.
        svc_rule(
            "transfer-reject-second-refer",
            &[],
            Match::request()
                .method("REFER")
                .direction(Direction::FromB)
                .filter(transfer_active),
            |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 491,
                    reason: "Request Pending".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        ),
        // ── transfer-http-reject / -error — /call/refer denied.
        svc_rule(
            "transfer-http-reject",
            &[],
            Match::internal_event()
                .topic("refer-http-result")
                .filter(|ctx| {
                    let outcome_ok = matches!(
                        ctx.event,
                        crate::event::CallEvent::InternalEvent { outcome, .. }
                            if outcome == "reject" || outcome == "error"
                    );
                    outcome_ok && phase(ctx) == Some(TransferPhase::ReferAuthorizing)
                }),
            |ctx| {
                let st = state(ctx)?;
                let leg = st.referrer_leg_id.clone();
                let (code, reason) = reject_code_reason(ctx);
                ok(vec![
                    notify(&leg, SUB_STATE_TERMINATED_NORESOURCE, code, &reason),
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                    RuleAction::SetTransfer { state: None },
                ])
            },
        ),
        // ── transfer-http-allow — /call/refer authorized → create C leg.
        svc_rule(
            "transfer-http-allow",
            &[],
            Match::internal_event()
                .topic("refer-http-result")
                .outcome("allow")
                .filter(|ctx| phase(ctx) == Some(TransferPhase::ReferAuthorizing)),
            |ctx| {
                let st = state(ctx)?.clone();
                let payload = match ctx.event {
                    crate::event::CallEvent::InternalEvent { payload, .. } => payload,
                    _ => return None,
                };
                let host = payload.get("destination").and_then(|d| d.get("host")).and_then(|v| v.as_str())?.to_string();
                let port = payload
                    .get("destination")
                    .and_then(|d| d.get("port"))
                    .and_then(|v| v.as_u64())
                    .map(|p| p as u16)
                    .unwrap_or(5060);
                let no_answer = payload.get("no_answer_timeout_sec").and_then(|v| v.as_i64());
                let callback_context = payload.get("callback_context").and_then(|v| v.as_str()).map(str::to_string);
                let new_refer_to = payload.get("new_refer_to").and_then(|v| v.as_str()).map(str::to_string);
                let header_updates: Vec<(String, Option<String>)> = payload
                    .get("update_headers")
                    .and_then(|v| v.as_object())
                    .map(|m| {
                        m.iter()
                            .map(|(k, v)| (k.clone(), v.as_str().map(str::to_string)))
                            .collect()
                    })
                    .unwrap_or_default();

                // Held SDP from A's INVITE snapshot (preserves codecs, port 0,
                // a=inactive). No profile → drop the body.
                let a_invite = super::relay::rebuild_a_leg_invite(ctx.call);
                let held = sip_message::extract_codec_profile(&a_invite.body).map(|profile| {
                    sip_message::build_held_sdp_from_profile(
                        &profile,
                        &sip_message::BuildHeldSdpOptions {
                            local_ip: ctx.config.sip_local_ip.clone(),
                            now_ms: ctx.now_ms,
                        },
                    )
                });
                // `Some(bytes)` set / `Some(empty)` drop. The C INVITE never
                // carries A's real SDP until the c-realign re-INVITE (5b).
                let body_override = Some(held.unwrap_or_default());

                let raw_refer_to = new_refer_to.unwrap_or_else(|| st.refer_to_uri.clone());
                let effective = to_bare_uri(&raw_refer_to);
                let c_leg_id = format!("b-{}", ctx.call.b_legs.len() + 1);

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
                        no_answer_timeout_sec: no_answer,
                        callback_context,
                        body_override,
                        header_updates,
                    },
                    RuleAction::SetTransfer { state: Some(new_state) },
                ])
            },
        ),
        // ── transfer-http-timeout — subscription-expiry fired (HTTP hung).
        svc_rule(
            "transfer-http-timeout",
            &[],
            Match::timer()
                .timer_type(call::TimerType::ReferSubscriptionExpiry)
                .filter(|ctx| phase(ctx) == Some(TransferPhase::ReferAuthorizing)),
            |ctx| {
                let st = state(ctx)?;
                let leg = st.referrer_leg_id.clone();
                ok(vec![
                    notify(&leg, SUB_STATE_TERMINATED_TIMEOUT, 500, "Server Internal Error"),
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                    RuleAction::SetTransfer { state: None },
                ])
            },
        ),
        // ── transfer-c-1xx-to-notify — C 1xx → NOTIFY active (deduped).
        svc_rule(
            "transfer-c-1xx-to-notify",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(1)
                .direction(Direction::FromB)
                .filter(|ctx| {
                    phase(ctx) == Some(TransferPhase::CRinging)
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            |ctx| {
                let st = state(ctx)?.clone();
                let resp = ctx.response()?;
                // Dedupe identical repeats against the *last* status only.
                if st.last_c_leg_notified_status == Some(resp.status) {
                    return ok(vec![]);
                }
                let leg = st.referrer_leg_id.clone();
                let mut new_state = st.clone();
                new_state.last_c_leg_notified_status = Some(resp.status);
                ok(vec![
                    notify(&leg, SUB_STATE_ACTIVE_60, resp.status, &resp.reason),
                    RuleAction::SetTransfer { state: Some(new_state) },
                ])
            },
        ),
        // ── transfer-c-200-initial — C answers its initial INVITE.
        svc_rule(
            "transfer-c-200-initial",
            &[],
            Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(|ctx| {
                    phase(ctx) == Some(TransferPhase::CRinging)
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            |ctx| {
                let st = state(ctx)?.clone();
                let resp = ctx.response()?;
                let c_leg_id = st.c_leg_id.clone()?;
                let leg = st.referrer_leg_id.clone();

                // Capture C's 200 SDP (drives the a-realign re-INVITE in 5b).
                let c_initial_sdp = (!resp.body.is_empty()).then(|| resp.body.clone());
                // A's SDP for the c-realign re-INVITE-C offer.
                let a_sdp = super::relay::rebuild_a_leg_invite(ctx.call).body;

                let mut new_state = st.clone();
                new_state.phase = TransferPhase::CRealigning;
                new_state.c_initial_sdp = c_initial_sdp;

                ok(vec![
                    RuleAction::UpdateLegState {
                        leg_id: c_leg_id.clone(),
                        state: LegState::Confirmed,
                        disposition: Some(call::LegDisposition::Bridged),
                    },
                    RuleAction::ConfirmDialog { leg_id: c_leg_id.clone() },
                    RuleAction::AckLeg { leg_id: c_leg_id.clone() },
                    notify(&leg, SUB_STATE_TERMINATED_NORESOURCE, 200, "OK"),
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::NoAnswer, Some(&c_leg_id)) },
                    RuleAction::ScheduleTimer {
                        timer_type: call::TimerType::ReferReinviteAnswer,
                        delay_sec: ctx.config.refer_reinvite_answer_sec,
                        leg_id: Some(c_leg_id.clone()),
                    },
                    RuleAction::SendReinvite {
                        leg_id: c_leg_id.clone(),
                        body: a_sdp,
                        add_headers: vec![],
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Answer,
                        leg_id: c_leg_id,
                        status_code: Some(200),
                        reason: None,
                    },
                    RuleAction::SetTransfer { state: Some(new_state) },
                ])
            },
        ),
        // ── transfer-c-fail-initial — C's initial INVITE 3xx–6xx.
        svc_rule(
            "transfer-c-fail-initial",
            &[],
            Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(|ctx| {
                    let is_fail = ctx.response().map(|r| r.status >= 300).unwrap_or(false);
                    is_fail
                        && phase(ctx) == Some(TransferPhase::CRinging)
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            |ctx| {
                let st = state(ctx)?.clone();
                let resp = ctx.response()?;
                let leg = st.referrer_leg_id.clone();
                let mut actions = vec![
                    notify(&leg, SUB_STATE_TERMINATED_NORESOURCE, resp.status, &resp.reason),
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                ];
                if let Some(c_leg_id) = st.c_leg_id.clone() {
                    actions.push(RuleAction::CancelTimer { id: timer_id(call::TimerType::NoAnswer, Some(&c_leg_id)) });
                    actions.push(RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: c_leg_id.clone(),
                        status_code: Some(resp.status as i64),
                        reason: Some(resp.reason.clone()),
                    });
                    actions.push(RuleAction::TerminateLeg {
                        leg_id: c_leg_id,
                        bye_disposition: Some(call::ByeDisposition::Rejected),
                    });
                }
                actions.push(RuleAction::SetTransfer { state: None });
                ok(actions)
            },
        ),
        // ── transfer-c-no-answer — C no-answer timer (beats CORE no-answer).
        svc_rule(
            "transfer-c-no-answer",
            &[],
            Match::timer()
                .timer_type(call::TimerType::NoAnswer)
                .filter(|ctx| {
                    let timer_leg = match ctx.event {
                        crate::event::CallEvent::Timer { leg_id, .. } => leg_id.as_deref(),
                        _ => None,
                    };
                    phase(ctx) == Some(TransferPhase::CRinging)
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()).is_some()
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == timer_leg
                }),
            |ctx| {
                let st = state(ctx)?.clone();
                let leg = st.referrer_leg_id.clone();
                let c_leg_id = st.c_leg_id.clone()?;
                ok(vec![
                    notify(&leg, SUB_STATE_TERMINATED_TIMEOUT, 408, "Request Timeout"),
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Timeout,
                        leg_id: c_leg_id.clone(),
                        status_code: None,
                        reason: Some("no_answer_timeout".to_string()),
                    },
                    RuleAction::DestroyLeg { leg_id: c_leg_id },
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                    RuleAction::SetTransfer { state: None },
                ])
            },
        ),
    ]
}
