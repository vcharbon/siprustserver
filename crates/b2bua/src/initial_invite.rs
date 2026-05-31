//! Initial-INVITE handling — port of `InitialInviteHandler.ts`. Builds the
//! decision request from the INVITE, calls the decision engine, and applies the
//! route (b-leg creation) or reject. This is an **async** handler (it calls the
//! decision backend), so it lives outside the synchronous rule chain; the router
//! invokes it for out-of-dialog INVITEs and runs the result through the same
//! invariant finalization as a rule result.

use std::net::SocketAddr;

use call::{
    ALegInviteSnapshot, Call, CallModelState, CdrEvent, CdrEventType, Leg, LegDisposition, LegKind,
    LegState, RemoteInfo,
};
use sip_message::message_helpers::{get_header, get_headers};
use sip_message::SipRequest;
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::decision::apply_route::apply_route;
use crate::decision::{CallDecisionEngine, NewCallRequest, NewCallResponse};
use crate::effects::{HandlerEffects, HandlerResult};
use crate::limiter::CallLimiter;
use crate::rules::relay;

/// Headers sent as top-level decision-request fields (excluded from `sip_headers`).
const STANDARD_HEADERS: &[&str] = &[
    "from", "to", "via", "contact", "content-type", "call-id", "cseq", "max-forwards",
    "content-length",
];

/// Build the initial [`Call`] (a-leg only) from an inbound INVITE. Pure.
pub fn build_initial_call(
    invite: &SipRequest,
    src: SocketAddr,
    config: &B2buaConfig,
    now_ms: i64,
) -> Call {
    let call_ref = call::derive_call_ref(
        &config.self_ordinal,
        &invite.call_id,
        invite.from.tag.as_deref().unwrap_or(""),
    );
    let a_leg = Leg {
        leg_id: "a".to_string(),
        call_id: invite.call_id.clone(),
        from_tag: invite.from.tag.clone().unwrap_or_default(),
        source: RemoteInfo {
            address: src.ip().to_string(),
            port: src.port(),
        },
        state: LegState::Trying,
        disposition: LegDisposition::Pending,
        dialogs: vec![],
        no_answer_timeout_sec: None,
        bye_disposition: None,
        local_uri: Some(invite.to.uri.clone()),
        remote_uri: Some(invite.from.uri.clone()),
        invite_request_uri: Some(invite.uri.clone()),
        pending_invite_txn: None,
        ext: None,
        kind: Some(LegKind::A),
        adopted: Some(false),
    };
    let a_leg_invite = ALegInviteSnapshot {
        uri: invite.uri.clone(),
        headers: invite
            .headers
            .iter()
            .map(|h| call::SipHeader {
                name: h.name.clone(),
                value: h.value.clone(),
            })
            .collect(),
        body: invite.body.clone(),
    };
    Call {
        call_ref,
        a_leg,
        b_legs: vec![],
        active_peer: None,
        callback_context: None,
        billing_context: None,
        a_leg_invite,
        limiter_entries: vec![],
        timers: vec![],
        cdr_events: vec![CdrEvent {
            event_type: CdrEventType::InviteReceived,
            timestamp: now_ms,
            leg_id: "a".to_string(),
            status_code: None,
            reason: None,
        }],
        state: CallModelState::Active,
        created_at: now_ms,
        a_leg_pending_vias: None,
        a_leg_pending_cseq: None,
        tag_map: vec![],
        trace_id: None,
        root_span_id: None,
        sampled: None,
        worker_index: None,
        topology: None,
        emergency: None,
        features: None,
        policy_update_headers: None,
        policy_update_body: None,
        active_rules: None,
        ext: None,
        message_count: Some(1),
        terminating_refresh_legs: None,
    }
}

/// Run the initial-INVITE decision + route/reject. `call` must already carry the
/// a-leg (from [`build_initial_call`]).
pub async fn handle_initial_invite(
    call: Call,
    decision: &dyn CallDecisionEngine,
    limiter: &dyn CallLimiter,
    config: &B2buaConfig,
    id_gen: &IdGen,
    now_ms: i64,
) -> HandlerResult {
    let a_invite = relay::rebuild_a_leg_invite(&call);
    let req = build_request(&a_invite);

    match decision.new_call(req).await {
        Ok(NewCallResponse::Route(route)) => {
            apply_route(call, route, &a_invite, limiter, config, id_gen, now_ms).await
        }
        Ok(NewCallResponse::Reject(reject)) => {
            reject_call(call, &a_invite, reject.reject_code, reject.reject_reason, id_gen, now_ms)
        }
        Err(_unavailable) => {
            reject_call(call, &a_invite, 503, Some("Service Unavailable".into()), id_gen, now_ms)
        }
    }
}

fn reject_call(
    mut call: Call,
    a_invite: &SipRequest,
    status: u16,
    reason: Option<String>,
    id_gen: &IdGen,
    now_ms: i64,
) -> HandlerResult {
    let reason = reason.unwrap_or_else(|| default_reason(status));
    // A non-100 final response needs a To-tag (the B2BUA's a-facing tag).
    let effect = relay::response_to_a_leg(
        a_invite,
        status,
        &reason,
        Some(id_gen.new_tag()),
        None,
        vec![],
        None,
        None,
    );
    call.cdr_events.push(CdrEvent {
        event_type: CdrEventType::Reject,
        timestamp: now_ms,
        leg_id: "a".to_string(),
        status_code: Some(status as i64),
        reason: Some(reason),
    });
    call.a_leg.state = LegState::Terminated;
    call.state = CallModelState::Terminated;
    let mut effects = HandlerEffects::new();
    effects.outbound.push(effect);
    HandlerResult { call, effects }
}

fn build_request(invite: &SipRequest) -> NewCallRequest {
    let sip_headers = invite
        .headers
        .iter()
        .filter(|h| !STANDARD_HEADERS.contains(&h.name.to_ascii_lowercase().as_str()))
        .map(|h| (h.name.clone(), h.value.clone()))
        .collect();
    NewCallRequest {
        call_id: invite.call_id.clone(),
        ruri: invite.uri.clone(),
        from: get_header(&invite.headers, "from").unwrap_or("").to_string(),
        to: get_header(&invite.headers, "to").unwrap_or("").to_string(),
        via: get_headers(&invite.headers, "via").iter().map(|s| s.to_string()).collect(),
        contact: get_header(&invite.headers, "contact").map(str::to_string),
        content_type: get_header(&invite.headers, "content-type").map(str::to_string),
        sip_headers,
        sip_body: (!invite.body.is_empty()).then(|| String::from_utf8_lossy(&invite.body).into_owned()),
    }
}

fn default_reason(status: u16) -> String {
    match status {
        403 => "Forbidden",
        404 => "Not Found",
        486 => "Busy Here",
        503 => "Service Unavailable",
        _ => "Declined",
    }
    .to_string()
}
