//! Initial-INVITE handling — port of `InitialInviteHandler.ts`. Builds the
//! decision request from the INVITE, calls the decision engine, and applies the
//! route (b-leg creation) or reject. This is an **async** handler (it calls the
//! decision backend), so it lives outside the synchronous rule chain; the router
//! invokes it for out-of-dialog INVITEs and runs the result through the same
//! invariant finalization as a rule result.

use std::net::SocketAddr;

use call::{
    ALegInviteSnapshot, Call, CallModelState, CallTopology, CdrEvent, CdrEventType, Leg,
    LegDisposition, LegKind, LegState, RemoteInfo,
};
use sip_message::message_helpers::{get_header, get_headers, parse_uri_params};
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

/// Derive the HA [`CallTopology`] from the proxy's stickiness cookie, carried as
/// URI params on the **topmost Record-Route** the front proxy inserted
/// (`w_pri`/`w_bak`/`e`/`v`/`kid`/`sig`; see `sip-proxy` `load_balancer.rs` /
/// `build_record_route_value`). The b2bua reads `w_pri`/`w_bak` so the backup
/// peer AGREES with the proxy's rendezvous (HRW 2nd-best) choice by construction
/// rather than re-deriving it (the proxy keys HRW off the alive-set + Call-ID the
/// b2bua cannot reproduce locally — see [`crate::repl::replication_target`]).
///
/// - Cookie present → `Some(CallTopology { pri: w_pri (or self_ordinal if the
///   param is absent/empty), bak: w_bak (may be empty), gen: 1 })`. A brand-new
///   call starts at `gen = 1`; the b2bua's update path bumps it per mutation (see
///   [`crate::store::CallState::update`]).
/// - No cookie (non-proxied / legacy INVITE) → `None`: the flush path then stays
///   non-replicating (`PutOpts::default()`), preserving today's behaviour.
fn topology_from_cookie(invite: &SipRequest, self_ordinal: &str) -> Option<CallTopology> {
    // The topmost Record-Route is the LAST proxy to add one on the request path
    // (proxies prepend), i.e. the FIRST Record-Route header here.
    let rr = get_header(&invite.headers, "record-route")?;
    let params = parse_uri_params(rr);
    // Treat it as a cookie only if it actually carries the stickiness grammar.
    if !params.contains_key("w_pri") && !params.contains_key("w_bak") {
        return None;
    }
    let pri = params
        .get("w_pri")
        .filter(|s| !s.is_empty())
        .cloned()
        .unwrap_or_else(|| self_ordinal.to_string());
    let bak = params.get("w_bak").cloned().unwrap_or_default();
    // Brand-new call: primary counter p = 1 (the primary "created" it), backup
    // counter b = 0 (no takeover yet). See `CallTopology` / ADR-0014.
    Some(CallTopology { pri, bak, gen: 1, bak_gen: 0 })
}

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
    let topology = topology_from_cookie(invite, &config.self_ordinal);
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
        topology,
        emergency: None,
        features: None,
        policy_update_headers: None,
        policy_update_body: None,
        active_rules: None,
        ext: None,
        message_count: Some(1),
        terminating_refresh_legs: None,
        relay_first_18x: None,
        promote_pem: None,
        transfer: None,
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
            apply_route(call, route, &a_invite, decision, limiter, config, id_gen, now_ms, 0).await
        }
        Ok(NewCallResponse::Reject(reject)) => {
            reject_call(call, &a_invite, reject.reject_code, reject.reject_reason, id_gen, now_ms)
        }
        Err(_unavailable) => {
            reject_call(call, &a_invite, 503, Some("Service Unavailable".into()), id_gen, now_ms)
        }
    }
}

pub(crate) fn reject_call(
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
        vec![],
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
