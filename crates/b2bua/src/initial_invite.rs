//! Initial-INVITE handling. Builds the decision request from the INVITE, calls
//! the decision engine, and applies the route (b-leg creation) or reject. This
//! is an **async** handler (it calls the decision backend), so it lives outside
//! the synchronous rule chain; the router invokes it for out-of-dialog INVITEs
//! and runs the result through the same invariant finalization as a rule
//! result. A setup its caller CANCELed before its turn ran asks the decision
//! engine nothing ([`cancelled_setup`]).

use std::net::SocketAddr;

use call::helpers::{add_cdr_event, mark_decision, record_termination};
use call::{
    ALegInviteSnapshot, Call, CallLimiterState, CallModelState, CdrEvent, CdrEventType,
    DecisionKind, Leg, LegKind, RemoteInfo, TerminationCause,
};
use sip_clock::Clock;
use sip_message::emergency::is_emergency_request;
use sip_message::generators::CapabilitySet;
use sip_message::header::{HeaderClass, HeaderName};
use sip_message::{SipHeader, SipMessage, SipRequest, SipStr};
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use b2bua_sdk::reason_phrase::default_reason;

use crate::decision::apply_reject::{apply_redirect, apply_reject};
use crate::decision::apply_route::apply_route;
use crate::decision::{
    CallDecisionEngine, CallDecisionError, NewCallRequest, NewCallResponse, RedirectContact,
    SipHeaderUpdates,
};
use crate::effects::{HandlerEffects, HandlerResult};
use crate::limiter::LimiterWorker;
use crate::rules::{relay, seed_services, ActionExecutor, ServiceDef};
use crate::trace;
use b2bua_sdk::event::CallEvent;

/// Headers sent as top-level decision-request fields (excluded from
/// `sip_headers`). Shared with the failure path: `route-failure` applies the
/// same exclusion to the failed final response's headers, so "the
/// non-structural remainder" means one thing across the decision seam.
///
/// `Max-Forwards` is deliberately NOT here: a decision that cannot see the hop
/// count cannot answer a spent one. It rides `sip_headers` read-only — the stack
/// owns what a leg it originates states ([`crate::rules::relay::build_b_leg`]).
pub(crate) const STANDARD_HEADERS: &[HeaderName] = &[
    HeaderName::From,
    HeaderName::To,
    HeaderName::Via,
    HeaderName::Contact,
    HeaderName::ContentType,
    HeaderName::CallId,
    HeaderName::CSeq,
    HeaderName::ContentLength,
];

/// Build the initial [`Call`] (a-leg only) from an inbound INVITE. Pure.
pub fn build_initial_call(
    invite: &SipRequest,
    src: SocketAddr,
    config: &B2buaConfig,
    id_gen: &IdGen,
    now_ms: i64,
) -> Call {
    let call_ref = call::derive_call_ref(
        &config.self_ordinal,
        invite.call_id().as_str(),
        invite.from().tag().unwrap_or(""),
    );
    // The limiter key is the call's incarnation: the call_ref plus a mark,
    // unique over time where the call_ref is not (a retried INVITE reuses its
    // Call-ID and From tag).
    let limiter =
        CallLimiterState::uncounted(call::derive_incarnation(&call_ref, &id_gen.new_tag()));
    let (local_addr, remote_addr) = relay::uas_addresses(invite);
    let a_leg = Leg {
        leg_id: "a".to_string(),
        call_id: invite.call_id().to_string(),
        source: RemoteInfo { address: src.ip().to_string(), port: src.port() },
        dialogs: Default::default(),
        local_uri: Some(local_addr),
        remote_uri: Some(remote_addr),
        invite_request_uri: Some(invite.request_uri().to_string()),
        outbound_invite: None,
        own_tag: None,
        ext: None,
        kind: Some(LegKind::A),
        // Derived from kind (the a-leg is always adopted); see `is_adopted`.
        adopted: None,
        invite_final_sent: None,
        answer_preempted: None,
        attempt: None,
        ring: call::Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: Default::default(),
    };
    let topology = topology_from_cookie(invite, &config.self_ordinal, &call_ref);
    let a_leg_invite = ALegInviteSnapshot {
        uri: invite.request_uri().to_string(),
        headers: invite
            .headers()
            .iter()
            .map(|h| call::SipHeader { name: h.name.to_string(), value: h.value.to_string() })
            .collect(),
        body: invite.body().to_vec(),
        cseq: invite.cseq().seq(),
        from_tag: invite.from().tag().unwrap_or_default().to_string(),
    };
    Call {
        call_ref,
        a_leg,
        b_legs: vec![],
        attempts: Vec::new(),
        active_peer: None,
        callback_context: None,
        billing_context: None,
        a_leg_invite,
        limiter,
        timers: vec![],
        timer_generation: 0,
        cdr_events: vec![CdrEvent {
            event_type: CdrEventType::InviteReceived,
            timestamp: now_ms,
            leg_id: "a".to_string(),
            status_code: None,
            reason: None,
            decision_ordinal: 0,
        }],
        state: CallModelState::Active,
        created_at: now_ms,
        trace_id: None,
        root_span_id: None,
        sampled: None,
        worker_index: None,
        topology,
        // A non-emergency INVITE writes *absence* (`None`), never `Some(false)`
        // — production only ever stamps `Some(true)` or leaves the field unset
        // (see `call` codec_roundtrip emergency contract). The field carries the
        // call's emergency state to the `;emerg=1`/`;em=1` URI/Via markers in
        // `stack_identity`, so it is derived from the INVITE here, not
        // hard-coded. The admission ladder classifies the wire request directly and
        // never read this field.
        emergency: is_emergency_request(invite).then_some(true),
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
        subscriptions: Vec::new(),
        reroute: None,
        provisionals: Vec::new(),
        message_seq: 0,
        decision_log: Vec::new(),
        decision_ordinal: 0,
        termination: None,
        message_turn: 1,
        sm_cursors: std::collections::BTreeMap::new(),
    }
}

/// The initial-INVITE turn of a setup its caller CANCELed before the turn ran:
/// the call opens its trace (ADR-0026), and nothing else happens — no
/// decision request, no limiter admit, no wire effect. The `Cancelled` turn
/// queued behind it ends the call with its Cancel CDR.
pub fn cancelled_setup(mut call: Call, invite_wire: &[u8], now_ms: i64) -> HandlerResult {
    let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
    trace::intake::activate(&mut call, &a_invite, invite_wire, now_ms);
    HandlerResult { call, effects: HandlerEffects::new() }
}

/// Run the initial-INVITE decision + route/reject. `call` must already carry the
/// a-leg (from [`build_initial_call`]); `invite_wire` is the datagram it arrived
/// as, which only the router holds (the call carries a header snapshot, not
/// bytes). `now_ms` is the turn's timestamp — every CDR entry and timer deadline
/// this handler mints reads it, so it is taken once; `clock` measures the
/// decision round trip, whose two ends are by definition not the same instant.
#[allow(clippy::too_many_arguments)]
pub async fn handle_initial_invite(
    mut call: Call,
    decision: &dyn CallDecisionEngine,
    limiter: &LimiterWorker,
    config: &B2buaConfig,
    id_gen: &IdGen,
    wire_faults: &crate::wire_faults::WireFaults,
    services: &[ServiceDef],
    invite_wire: &[u8],
    clock: &Clock,
    now_ms: i64,
) -> HandlerResult {
    let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let req = build_request(&a_invite);

    // ── Trace intake (ADR-0026) ────────────────────────────────────────────
    // The admission chain runs here, once, before the decision round trip, so a
    // sampled call's story starts at its own INVITE. `apply_route` owns the
    // other door (an engine force-enable, on this route or a later failover
    // one), and it backfills what it missed.
    trace::intake::activate(&mut call, &a_invite, invite_wire, now_ms);
    let mut request_json = trace::intake::request_body(&call, &req);

    let sent_at_ms = clock.now_ms();
    let response = decision.new_call(req).await;
    let received_at_ms = clock.now_ms();
    if let Ok(NewCallResponse::Route(route)) = &response {
        // The route that turned the trace on arrived after its own request was
        // sent, so rebuild that request: it is derived from the a-leg INVITE, so
        // the rebuilt body IS the body that went out.
        if route.trace && trace::intake::force_enable(&mut call, invite_wire, now_ms) {
            request_json = Some(trace::intake::json_body(&build_request(&a_invite)));
        }
    }
    trace::intake::record_new_call(&call, request_json, &response, sent_at_ms, received_at_ms);

    match response {
        Ok(NewCallResponse::Route(route)) => {
            // The route built the Call; now run each service's `init` (ADR-0016
            // X8), folding any seed (cursor + data + initial actions) through the
            // normal executor.
            // A dormant service (`init` → `None`) and the empty-service list
            // both leave the result untouched.
            let result = apply_route(
                call,
                route,
                &a_invite,
                invite_wire,
                decision,
                limiter,
                config,
                id_gen,
                clock,
                now_ms,
                0,
            )
            .await;
            let exec = ActionExecutor { config, id_gen, now_ms, wire_faults };
            let setup_event = setup_event(&result.call, &a_invite);
            seed_services(result, services, &exec, &setup_event, "a", call::Direction::FromA)
        }
        Ok(NewCallResponse::Reject(reject)) => apply_reject(
            call,
            reject,
            DecisionKind::Reject,
            Some("a".into()),
            &a_invite,
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
        ),
        Ok(NewCallResponse::Redirect(rd)) => apply_redirect(
            call,
            rd,
            DecisionKind::Redirect,
            Some("a".into()),
            &a_invite,
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
        ),
        // `Relay` is a failover-only treatment; with no captured downstream
        // failure at new-call time it falls back to 480 (ADR-0017 X5).
        Ok(NewCallResponse::Relay { label }) => {
            let call = mark_decision(call, now_ms, DecisionKind::Relay, Some("a".into()), label);
            reject_call(
                call,
                &a_invite,
                480,
                Some("Temporarily Unavailable".into()),
                None,
                &[],
                &config.minted_final_advertisement,
                id_gen,
                now_ms,
                TerminationCause::DecisionReject,
            )
        }
        // No decision was returned: nothing to record, the stack's own final —
        // the one the engine stated, else 503.
        Err(CallDecisionError::Refused { code, reason, update_headers }) => reject_call(
            call,
            &a_invite,
            code,
            reason,
            update_headers.as_ref(),
            &[],
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
            TerminationCause::Admission,
        ),
        Err(CallDecisionError::Unavailable(_)) => reject_call(
            call,
            &a_invite,
            503,
            Some("Service Unavailable".into()),
            None,
            &[],
            &config.minted_final_advertisement,
            id_gen,
            now_ms,
            TerminationCause::Admission,
        ),
    }
}

/// Synthesize the setup `CallEvent` services' init actions resolve against (the
/// original a-leg INVITE on the a-leg). The `src` is the caller's address from
/// the a-leg, defaulting to `0.0.0.0:0` if it is not an `ip:port` (init actions
/// resolve legs from the call, not the event source).
fn setup_event(call: &Call, a_invite: &SipRequest) -> CallEvent {
    let src: SocketAddr = format!("{}:{}", call.a_leg.source.address, call.a_leg.source.port)
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 0)));
    CallEvent::Sip {
        message: Box::new(SipMessage::Request(a_invite.clone())),
        src,
        matched_client_txn: false,
    }
}

/// Answer the a-leg with a final failure / redirect the decision layer authors,
/// or one this stack authors on its own account (an unavailable backend, a
/// refused target). `update_headers` adds non-structural headers (e.g.
/// `Reason:`, RFC 3326); `contacts` renders one `Contact: <uri>;q=…` header
/// per entry (used for a 3xx redirect — ADR-0017). Structural headers are
/// skipped (the generator owns them, header-ownership matrix X2). Marks no
/// decision: the caller marks the one it applies, and a final of the stack's
/// own is none. `cause` is the termination record's: the decision layer's
/// refusal, or the stack's own admission. A failure final carries the
/// deployment's `advertisement` for a minted final under the decision's
/// statements.
#[allow(clippy::too_many_arguments)]
pub(crate) fn reject_call(
    mut call: Call,
    a_invite: &SipRequest,
    status: u16,
    reason: Option<String>,
    update_headers: Option<&SipHeaderUpdates>,
    contacts: &[RedirectContact],
    advertisement: &CapabilitySet,
    id_gen: &IdGen,
    now_ms: i64,
    cause: TerminationCause,
) -> HandlerResult {
    let reason = reason.unwrap_or_else(|| default_reason(status).to_string());
    // A redirect whose target does not read cannot be authored: the caller dials
    // what a 3xx Contact names, so an invented one sends it at an address the
    // decision never stated. Answer the plain server error instead — the
    // re-entry carries no contacts, so it always terminates.
    let mut extra_headers = match build_reject_headers(update_headers, contacts) {
        Ok(headers) => headers,
        Err(err) => {
            tracing::warn!(
                call_ref = %call.call_ref,
                detail = %err.detail(),
                "redirect refused"
            );
            return reject_call(
                call,
                a_invite,
                500,
                Some(err.to_string()),
                update_headers,
                &[],
                advertisement,
                id_gen,
                now_ms,
                TerminationCause::Admission,
            );
        }
    };
    // A failure final refused in this element's own name carries the
    // deployment's advertisement, each half under the decision's statements.
    if crate::rules::capabilities::is_minted_failure(status) {
        crate::rules::capabilities::stamp_own_advertisement(
            advertisement,
            |name| update_headers.is_some_and(|u| u.keys().any(|n| name.matches(n))),
            &mut extra_headers,
        );
    }
    let mut effects = HandlerEffects::new();
    // A non-100 final response needs a To-tag (the B2BUA's a-facing tag).
    if let Some(effect) = relay::response_to_a_leg(
        &mut call,
        &mut effects,
        a_invite,
        status,
        &reason,
        Some(id_gen.new_tag()),
        None,
        vec![],
        None,
        None,
        extra_headers,
        crate::effects::Provenance::Authored,
        crate::rules::relay::Author::Stack,
    ) {
        let mut effect = effect;
        let adds = update_headers.map(b2bua_sdk::header_update::header_adds).unwrap_or_default();
        if let crate::effects::OutboundBody::Response(resp) = &mut effect.body {
            *resp = crate::rules::stated_headers::response_with_adds(resp.clone(), &adds);
        }
        effects.outbound.push(effect);
        call = add_cdr_event(
            call,
            CdrEvent {
                event_type: CdrEventType::Reject,
                timestamp: now_ms,
                leg_id: "a".to_string(),
                status_code: Some(status as i64),
                reason: Some(reason),
                decision_ordinal: 0,
            },
        );
    }
    call = record_termination(call, now_ms, cause, None);
    call.state = CallModelState::Terminated;
    HandlerResult { call, effects }
}

fn build_request(invite: &SipRequest) -> NewCallRequest {
    // One `(name, value)` per wire line, in wire order: the order of a multi-line
    // header is part of its meaning (RFC 7044 `History-Info` index chains), whatever
    // spelling each line wears.
    let sip_headers: Vec<(String, String)> = invite
        .headers()
        .iter()
        .filter(|h| !STANDARD_HEADERS.iter().any(|n| n.matches(&h.name)))
        .map(|h| (h.name.to_string(), h.value.to_string()))
        .collect();
    NewCallRequest {
        call_id: invite.call_id().as_str().to_string(),
        ruri: invite.request_uri().to_string(),
        from: invite.raw(HeaderName::From).next().unwrap_or("").to_string(),
        to: invite.raw(HeaderName::To).next().unwrap_or("").to_string(),
        via: invite.raw(HeaderName::Via).map(str::to_string).collect(),
        contact: invite.raw(HeaderName::Contact).map(str::to_string).collect(),
        content_type: invite.raw(HeaderName::ContentType).next().map(str::to_string),
        sip_headers,
        sip_body: (!invite.body().is_empty()).then(|| invite.body().to_vec()),
    }
}

/// Build the extra response headers for a reject/redirect: the non-structural
/// `update_headers` *sets* (e.g. `Reason:`), one line per stated line — its
/// adds yield to the final as built ([`reject_call`]) — plus one
/// `Contact: <uri>;q=…` per redirect target. Removals and stack-owned keys are dropped — the response
/// generator owns the structural set (ADR-0017 X2), including the Contact a
/// redirect authors from its typed target list.
/// Errs when a redirect target does not read — the whole redirect is refused,
/// never partially authored (dropping the unreadable entry would send the caller
/// at whichever targets happened to parse).
fn build_reject_headers(
    update_headers: Option<&SipHeaderUpdates>,
    contacts: &[RedirectContact],
) -> Result<Vec<SipHeader>, relay::UnreadableAddress> {
    let mut out: Vec<SipHeader> = Vec::new();
    if let Some(map) = update_headers {
        for (name, val) in map {
            let named = HeaderName::from(name.as_str());
            if named.class() != HeaderClass::EndToEnd {
                continue;
            }
            if val.adds() {
                continue;
            }
            for v in val.lines() {
                out.push(SipHeader { name: SipStr::owned(name), value: SipStr::owned(v) });
            }
        }
    }
    for c in contacts {
        out.push(relay::redirect_contact(&c.uri, c.q)?);
    }
    Ok(out)
}

mod topology_cookie;
pub(crate) use topology_cookie::foreign_primary;
use topology_cookie::topology_from_cookie;

#[cfg(test)]
mod emergency_on_invite_tests;

#[cfg(test)]
mod stickiness_cookie_tests;

#[cfg(test)]
mod multi_instance_header_tests;
