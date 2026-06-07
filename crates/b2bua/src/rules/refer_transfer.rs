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
//! no-answer) portion. Slice 5b adds the c-realign phase: `transfer-c-realign-200`
//! (→ a-realigning + re-INVITE A with C's active answer), `transfer-c-realign-fail`
//! / `transfer-c-realign-timeout` (rollback), and the realigning gating rules
//! `transfer-c-glare-reinvite` (491) + `transfer-b-in-cre-are-reject` (481).
//! Slice 5c adds the a-realign / merge phase: `transfer-a-realign-200` (ACK A,
//! `merge(a, cLegId)`, clear slice — transfer complete), `transfer-a-realign-fail`
//! / `transfer-a-realign-timeout` (rollback), `transfer-a-glare-reinvite` (491),
//! and the cross-phase `transfer-overall-timeout` (the 120s safety watchdog →
//! rollback). A BYE from A while a-realigning rides the CORE `relay-bye` path,
//! whose begin-termination BYEs the orphaned B + C — no dedicated rule needed.

use b2bua_sdk::{define_service, sm_rule};
use call::{
    Call, CdrEventType, Direction, LegState, StateLabel, TransferPhase, TransferState,
};
use sip_message::message_helpers::get_header;
use sip_message::sipfrag::sipfrag_from_status;
use sip_message::Method;

use super::model::{
    Effect, Match, RuleAction, RuleContext, RuleDefinition, CORE_LAYER,
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
    RuleDefinition::core(id, CORE_LAYER, overrides, matcher, handle)
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

// The `transfer` callflow service (ADR-0016): `TransferPhase` is the declared
// machine, its cursor a read-only *projection* of the authoritative
// `Call.transfer.phase` (see `project_cursor`, mirroring the `global-call`
// projection). Each rule is gated by `active_states` instead of a `phase(ctx)`
// match-column; the `transitions` it declares are the diagram edges (the cursor
// is moved by the projection in `finalize`, so they are documentation, never
// enforced — like `global-call`). The handlers are unchanged: they still write
// `Call.transfer.phase` via `SetTransfer`, which the projection mirrors.
//
// `init` stays dormant (`None`): transfer is triggered by an in-dialog REFER
// mid-call (the machine-less seed rules in `transfer_seed_rules`), not at INVITE
// setup. The cursor first appears when the seed installs the slice.
define_service! {
    id: "transfer",
    machine: TRANSFER_MACHINE,
    states: Phase { ReferAuthorizing, CRinging, CRealigning, ARealigning },
    init: |_call| None,
    rules: [
        // ── transfer-reject-second-refer — a second REFER while active → 491.
        sm_rule! {
            id: "transfer-reject-second-refer",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ReferAuthorizing, Phase::CRinging, Phase::CRealigning, Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::Respond { status: 491, label: "491 → B (second REFER pending)" },
            ],
            matcher: Match::request()
                .method("REFER")
                .direction(Direction::FromB),
            handle: |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 491,
                    reason: "Request Pending".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        },
        // ── transfer-http-reject / -error — /call/refer denied.
        sm_rule! {
            id: "transfer-http-reject",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ReferAuthorizing ],
            transitions: [],
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
                let leg = st.referrer_leg_id.clone();
                let (code, reason) = reject_code_reason(ctx);
                ok(vec![
                    notify(&leg, SUB_STATE_TERMINATED_NORESOURCE, code, &reason),
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferSubscriptionExpiry, None) },
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                    RuleAction::SetTransfer { state: None },
                ])
            },
        },
        // ── transfer-http-allow — /call/refer authorized → create C leg.
        sm_rule! {
            id: "transfer-http-allow",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ReferAuthorizing ],
            transitions: [ Phase::ReferAuthorizing => Phase::CRinging ],
            effects: [
                Effect::Originate { method: Method::Invite, label: "INVITE → C (transfer target)" },
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
                        kind: None,
                    },
                    RuleAction::SetTransfer { state: Some(new_state) },
                ])
            },
        },
        // ── transfer-http-timeout — subscription-expiry fired (HTTP hung).
        sm_rule! {
            id: "transfer-http-timeout",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ReferAuthorizing ],
            transitions: [],
            effects: [
                Effect::Originate { method: Method::Notify, label: "NOTIFY terminated;timeout → referrer" },
                Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
            ],
            matcher: Match::timer()
                .timer_type(call::TimerType::ReferSubscriptionExpiry),
            handle: |ctx| {
                let st = state(ctx)?;
                let leg = st.referrer_leg_id.clone();
                ok(vec![
                    notify(&leg, SUB_STATE_TERMINATED_TIMEOUT, 500, "Server Internal Error"),
                    RuleAction::CancelTimer { id: timer_id(call::TimerType::ReferOverallSafety, None) },
                    RuleAction::SetTransfer { state: None },
                ])
            },
        },
        // ── transfer-c-1xx-to-notify — C 1xx → NOTIFY active (deduped).
        sm_rule! {
            id: "transfer-c-1xx-to-notify",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRinging ],
            transitions: [],
            effects: [
                Effect::Originate { method: Method::Notify, label: "NOTIFY active (C progress) → referrer" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(1)
                .direction(Direction::FromB)
                .filter(|ctx| {
                    state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            handle: |ctx| {
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
        },
        // ── transfer-c-200-initial — C answers its initial INVITE.
        sm_rule! {
            id: "transfer-c-200-initial",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRinging ],
            transitions: [ Phase::CRinging => Phase::CRealigning ],
            effects: [
                Effect::Originate { method: Method::Ack, label: "ACK → C (answer initial INVITE)" },
                Effect::Originate { method: Method::Notify, label: "NOTIFY terminated → referrer" },
                Effect::Originate { method: Method::Invite, label: "re-INVITE → C (c-realign, A's SDP)" },
                Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "arm c-realign re-INVITE watchdog" },
                Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry" },
                Effect::GuardTimer { timer: call::TimerType::NoAnswer, label: "cancel C no-answer" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(|ctx| {
                    state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            handle: |ctx| {
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
        },
        // ── transfer-c-fail-initial — C's initial INVITE 3xx–6xx.
        sm_rule! {
            id: "transfer-c-fail-initial",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRinging ],
            transitions: [],
            effects: [
                Effect::Originate { method: Method::Notify, label: "NOTIFY terminated (C failed) → referrer" },
                Effect::Originate { method: Method::Bye, label: "BYE → C (terminate failed leg)" },
                Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry + overall + C no-answer" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .leg_states(&[LegState::Trying, LegState::Early])
                .filter(|ctx| {
                    let is_fail = ctx.response().map(|r| r.status >= 300).unwrap_or(false);
                    is_fail
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            handle: |ctx| {
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
        },
        // ── transfer-c-no-answer — C no-answer timer (beats CORE no-answer).
        sm_rule! {
            id: "transfer-c-no-answer",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRinging ],
            transitions: [],
            effects: [
                Effect::Originate { method: Method::Notify, label: "NOTIFY terminated;timeout → referrer" },
                Effect::Originate { method: Method::Bye, label: "BYE → C (no answer)" },
                Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry + overall" },
            ],
            matcher: Match::timer()
                .timer_type(call::TimerType::NoAnswer)
                .filter(|ctx| {
                    let timer_leg = match ctx.event {
                        crate::event::CallEvent::Timer { leg_id, .. } => leg_id.as_deref(),
                        _ => None,
                    };
                    state(ctx).and_then(|s| s.c_leg_id.as_deref()).is_some()
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == timer_leg
                }),
            handle: |ctx| {
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
        },
        // ── transfer-c-realign-200 — C answers the c-realign re-INVITE (200).
        // Distinguished from `transfer-c-200-initial` by `legState: confirmed`
        // (the initial INVITE answered trying/early). referTransfer.ts:476-522.
        sm_rule! {
            id: "transfer-c-realign-200",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRealigning ],
            transitions: [ Phase::CRealigning => Phase::ARealigning ],
            effects: [
                Effect::Originate { method: Method::Ack, label: "ACK → C (c-realign answered)" },
                Effect::Originate { method: Method::Invite, label: "re-INVITE → A (a-realign, C's active SDP)" },
                Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "cancel C / arm A re-INVITE watchdog" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromB)
                .leg_states(&[LegState::Confirmed])
                .filter(|ctx| {
                    state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            handle: |ctx| {
                let st = state(ctx)?.clone();
                let resp = ctx.response()?;
                let c_leg_id = st.c_leg_id.clone()?;

                // Offer A the active SDP C just answered on the c-realign
                // re-INVITE (sendrecv, C's real port/codec) so A enables its
                // send path. C's *initial* held answer would leave A inactive →
                // one-way audio (referTransfer.ts:495-497, the load-bearing note).
                let c_realign_sdp = resp.body.clone();

                let mut new_state = st.clone();
                new_state.phase = TransferPhase::ARealigning;

                ok(vec![
                    RuleAction::AckLeg { leg_id: c_leg_id.clone() },
                    RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferReinviteAnswer, Some(&c_leg_id)),
                    },
                    RuleAction::ScheduleTimer {
                        timer_type: call::TimerType::ReferReinviteAnswer,
                        delay_sec: ctx.config.refer_reinvite_answer_sec,
                        leg_id: Some("a".to_string()),
                    },
                    RuleAction::SendReinvite {
                        leg_id: "a".to_string(),
                        body: c_realign_sdp,
                        add_headers: vec![],
                    },
                    RuleAction::SetTransfer { state: Some(new_state) },
                ])
            },
        },
        // ── transfer-c-realign-fail — C rejects the c-realign re-INVITE → rollback.
        // `legState: confirmed` distinguishes this from `transfer-c-fail-initial`.
        // begin-termination BYEs all three confirmed legs (A, B, C). The slice is
        // NOT cleared — the call termination path drops it. referTransfer.ts:526-568.
        sm_rule! {
            id: "transfer-c-realign-fail",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRealigning ],
            transitions: [],
            effects: [
                Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel re-INVITE watchdog + overall" },
                Effect::LifecycleCommand { label: "terminate (c-realign rollback — BYE A/B/C)" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .direction(Direction::FromB)
                .leg_states(&[LegState::Confirmed])
                .filter(|ctx| {
                    let is_fail = ctx.response().map(|r| r.status >= 300).unwrap_or(false);
                    is_fail
                        && state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            handle: |ctx| {
                let st = state(ctx)?.clone();
                let resp = ctx.response()?;
                let mut actions = vec![];
                if let Some(c_leg_id) = st.c_leg_id.clone() {
                    actions.push(RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferReinviteAnswer, Some(&c_leg_id)),
                    });
                }
                actions.push(RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferOverallSafety, None),
                });
                if let Some(c_leg_id) = st.c_leg_id.clone() {
                    actions.push(RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: c_leg_id,
                        status_code: Some(resp.status as i64),
                        reason: Some("transfer-rollback-c-realign".to_string()),
                    });
                }
                actions.push(RuleAction::BeginTermination { reason: None });
                ok(actions)
            },
        },
        // ── transfer-c-realign-timeout — `refer_reinvite_answer` fired while
        // c-realigning (C never answered the re-INVITE) → rollback. Shares the
        // timer type with `transfer-a-realign-timeout` (5c); the active-state gate
        // keeps them mutually exclusive. referTransfer.ts:572-598.
        sm_rule! {
            id: "transfer-c-realign-timeout",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRealigning ],
            transitions: [],
            effects: [
                Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
                Effect::LifecycleCommand { label: "terminate (c-realign timeout rollback)" },
            ],
            matcher: Match::timer()
                .timer_type(call::TimerType::ReferReinviteAnswer),
            handle: |ctx| {
                let st = state(ctx)?.clone();
                let mut actions = vec![RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferOverallSafety, None),
                }];
                if let Some(c_leg_id) = st.c_leg_id.clone() {
                    actions.push(RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Timeout,
                        leg_id: c_leg_id,
                        status_code: None,
                        reason: Some("transfer-rollback-c-realign".to_string()),
                    });
                }
                actions.push(RuleAction::BeginTermination { reason: None });
                ok(actions)
            },
        },
        // ── transfer-c-glare-reinvite — C re-INVITEs during realigning → 491.
        // Beats CORE `reinvite-glare`/`relay-reinvite` by SERVICE_LAYER.
        // referTransfer.ts:602-616.
        sm_rule! {
            id: "transfer-c-glare-reinvite",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRealigning, Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::Respond { status: 491, label: "491 → C (glare during realign)" },
            ],
            matcher: Match::request()
                .method("INVITE")
                .direction(Direction::FromB)
                .filter(|ctx| {
                    state(ctx).and_then(|s| s.c_leg_id.as_deref()) == Some(ctx.source_leg_id)
                }),
            handle: |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 491,
                    reason: "Request Pending".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        },
        // ── transfer-a-realign-200 — A answers the a-realign re-INVITE (200) →
        // merge(a, c). The transfer is complete: ACK A, cancel A's
        // `refer_reinvite_answer` + the overall-safety timer, `merge(a, cLegId)`
        // (A↔C now bridged), CDR answer "transfer-completed", and clear the slice.
        // B is left an orphan confirmed leg — a subsequent A BYE → begin-termination
        // BYEs both B and C. referTransfer.ts:620-654.
        sm_rule! {
            id: "transfer-a-realign-200",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::Originate { method: Method::Ack, label: "ACK → A (a-realign answered)" },
                Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "cancel A re-INVITE watchdog + overall" },
                Effect::LifecycleCommand { label: "merge A↔C (transfer complete)" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .status_class(2)
                .direction(Direction::FromA)
                .filter(|ctx| ctx.source_leg_id == "a"),
            handle: |ctx| {
                let st = state(ctx)?.clone();
                let c_leg_id = st.c_leg_id.clone()?;
                ok(vec![
                    RuleAction::AckLeg { leg_id: "a".to_string() },
                    RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferReinviteAnswer, Some("a")),
                    },
                    RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferOverallSafety, None),
                    },
                    RuleAction::Merge {
                        leg_a: "a".to_string(),
                        leg_b: c_leg_id,
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Answer,
                        leg_id: "a".to_string(),
                        status_code: Some(200),
                        reason: Some("transfer-completed".to_string()),
                    },
                    RuleAction::SetTransfer { state: None },
                ])
            },
        },
        // ── transfer-a-realign-fail — A rejects the a-realign re-INVITE → rollback.
        // begin-termination BYEs all three confirmed legs (A, B, C); the slice is
        // dropped as the call terminates. referTransfer.ts:658-690.
        sm_rule! {
            id: "transfer-a-realign-fail",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::GuardTimer { timer: call::TimerType::ReferReinviteAnswer, label: "cancel A re-INVITE watchdog + overall" },
                Effect::LifecycleCommand { label: "terminate (a-realign rollback — BYE A/B/C)" },
            ],
            matcher: Match::response()
                .method("INVITE")
                .direction(Direction::FromA)
                .filter(|ctx| {
                    let is_fail = ctx.response().map(|r| r.status >= 300).unwrap_or(false);
                    is_fail && ctx.source_leg_id == "a"
                }),
            handle: |ctx| {
                let resp = ctx.response()?;
                ok(vec![
                    RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferReinviteAnswer, Some("a")),
                    },
                    RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferOverallSafety, None),
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Reject,
                        leg_id: "a".to_string(),
                        status_code: Some(resp.status as i64),
                        reason: Some("transfer-rollback-a-realign".to_string()),
                    },
                    RuleAction::BeginTermination { reason: None },
                ])
            },
        },
        // ── transfer-a-realign-timeout — `refer_reinvite_answer` fired while
        // a-realigning (A never answered) → rollback. Shares the timer type with
        // `transfer-c-realign-timeout`; the active-state gate + the fired timer's
        // leg=="a" keep them mutually exclusive. referTransfer.ts:694-720.
        sm_rule! {
            id: "transfer-a-realign-timeout",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::GuardTimer { timer: call::TimerType::ReferOverallSafety, label: "cancel overall-safety" },
                Effect::LifecycleCommand { label: "terminate (a-realign timeout rollback)" },
            ],
            matcher: Match::timer()
                .timer_type(call::TimerType::ReferReinviteAnswer)
                .filter(|ctx| {
                    let timer_leg = match ctx.event {
                        crate::event::CallEvent::Timer { leg_id, .. } => leg_id.as_deref(),
                        _ => None,
                    };
                    timer_leg == Some("a")
                }),
            handle: |_ctx| {
                ok(vec![
                    RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferOverallSafety, None),
                    },
                    RuleAction::AddCdrEvent {
                        event_type: CdrEventType::Timeout,
                        leg_id: "a".to_string(),
                        status_code: None,
                        reason: Some("transfer-rollback-a-realign".to_string()),
                    },
                    RuleAction::BeginTermination { reason: None },
                ])
            },
        },
        // ── transfer-a-glare-reinvite — A re-INVITEs during realigning → 491.
        // Beats CORE `relay-reinvite` by SERVICE_LAYER. referTransfer.ts:724-737.
        sm_rule! {
            id: "transfer-a-glare-reinvite",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRealigning, Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::Respond { status: 491, label: "491 → A (glare during realign)" },
            ],
            matcher: Match::request()
                .method("INVITE")
                .direction(Direction::FromA),
            handle: |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 491,
                    reason: "Request Pending".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        },
        // ── transfer-overall-timeout — the overall-safety watchdog fired in any
        // of the four phases → rollback. Cancel the sub-expiry + both possible
        // `refer_reinvite_answer` ids (C's and A's), CDR timeout, begin-termination.
        // referTransfer.ts:760-794.
        sm_rule! {
            id: "transfer-overall-timeout",
            machine: TRANSFER_MACHINE,
            active: [ Phase::ReferAuthorizing, Phase::CRinging, Phase::CRealigning, Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::GuardTimer { timer: call::TimerType::ReferSubscriptionExpiry, label: "cancel subscription-expiry + both re-INVITE watchdogs" },
                Effect::LifecycleCommand { label: "terminate (overall-safety watchdog rollback)" },
            ],
            matcher: Match::timer()
                .timer_type(call::TimerType::ReferOverallSafety),
            handle: |ctx| {
                let st = state(ctx)?.clone();
                let mut actions = vec![RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferSubscriptionExpiry, None),
                }];
                if let Some(c_leg_id) = st.c_leg_id.clone() {
                    actions.push(RuleAction::CancelTimer {
                        id: timer_id(call::TimerType::ReferReinviteAnswer, Some(&c_leg_id)),
                    });
                }
                actions.push(RuleAction::CancelTimer {
                    id: timer_id(call::TimerType::ReferReinviteAnswer, Some("a")),
                });
                actions.push(RuleAction::AddCdrEvent {
                    event_type: CdrEventType::Timeout,
                    leg_id: "a".to_string(),
                    status_code: None,
                    reason: Some("transfer-overall-timeout".to_string()),
                });
                actions.push(RuleAction::BeginTermination { reason: None });
                ok(actions)
            },
        },
        // ── transfer-b-in-cre-are-reject — referrer B's non-BYE in-dialog request
        // during realigning → 481 (B's signalling is "dead" until merge; its BYE
        // is still allowed through `relay-bye`). referTransfer.ts:741-756.
        sm_rule! {
            id: "transfer-b-in-cre-are-reject",
            machine: TRANSFER_MACHINE,
            active: [ Phase::CRealigning, Phase::ARealigning ],
            transitions: [],
            effects: [
                Effect::Respond { status: 481, label: "481 → B (referrer signalling dead until merge)" },
            ],
            matcher: Match::request()
                .direction(Direction::FromB)
                .filter(|ctx| {
                    let method_ok = ctx
                        .request()
                        .map(|r| r.method != "BYE")
                        .unwrap_or(false);
                    method_ok
                        && state(ctx).map(|s| s.referrer_leg_id.as_str()) == Some(ctx.source_leg_id)
                }),
            handle: |_ctx| {
                ok(vec![RuleAction::Respond {
                    status: 481,
                    reason: "Call/Transaction Does Not Exist".to_string(),
                    body: vec![],
                    content_type: None,
                }])
            },
        },
    ],
}

/// The machine-gated service rules, kept under the pre-retrofit name for
/// `default_rules()` (the engine runs them via the flat rule list; the
/// `define_service!`-generated `rules()` is the source).
pub fn transfer_rules() -> Vec<RuleDefinition> {
    rules()
}

/// The `transfer` service descriptor — registered in the doc-generator registry
/// (`b2bua-runner::compose_services`) so `docs/sm/transfer.md` is generated from
/// the same declared `active_states`/`transitions` the engine gates on.
pub fn transfer_service_def() -> crate::rules::ServiceDef {
    service_def()
}

/// Project the authoritative `Call.transfer.phase` into the `transfer` machine
/// cursor (ADR-0016), mirroring the `global-call` projection: the cursor is a
/// read-only view the machine-gated rules select on, while `Call.transfer` stays
/// the single source of truth. Called from `invariants::finalize`. Clearing the
/// slice removes the cursor, deactivating the machine (so post-transfer relay is
/// no longer intercepted by the glare/realign rules).
pub fn project_cursor(call: &mut Call) {
    match call.transfer.as_ref().map(|t| t.phase) {
        Some(p) => {
            call.sm_cursors.insert(TRANSFER_MACHINE, phase_label(p));
        }
        None => {
            call.sm_cursors.remove(&TRANSFER_MACHINE);
        }
    }
}

fn phase_label(p: TransferPhase) -> StateLabel {
    match p {
        TransferPhase::ReferAuthorizing => Phase::ReferAuthorizing.label(),
        TransferPhase::CRinging => Phase::CRinging.label(),
        TransferPhase::CRealigning => Phase::CRealigning.label(),
        TransferPhase::ARealigning => Phase::ARealigning.label(),
    }
}
