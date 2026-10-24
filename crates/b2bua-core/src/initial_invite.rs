//! The initial INVITE's folds (ADR-0052 §7): the call built from it, the
//! decision request built from it, the treatment the consult's answer states,
//! the stack's own finals and the setup event services seed against. The
//! consult, the trace intake and the clock readings around them are the
//! shell's (`initial_consult`).

use std::net::SocketAddr;

use call::helpers::{add_cdr_event, mark_decision, record_termination};
use call::{
    ALegInviteSnapshot, Call, CallLimiterState, CallModelState, CdrEvent, CdrEventType,
    DecisionKind, Leg, LegKind, RemoteInfo, TerminationCause,
};
use sip_ids::TurnIds;
use sip_message::emergency::is_emergency_request;
use sip_message::generators::CapabilitySet;
use sip_message::header::{HeaderClass, HeaderName};
use sip_message::{SipHeader, SipMessage, SipRequest, SipStr};

use crate::config::B2buaConfig;
use b2bua_sdk::reason_phrase::default_reason;

use crate::decision::apply_reject::{apply_redirect, apply_reject};
use crate::decision::{
    CallDecisionError, NewCallRequest, NewCallResponse, RedirectContact, RouteDecision,
    SipHeaderUpdates,
};
use crate::effects::{BufferedObservabilityEffect, HandlerEffects, HandlerResult};
use crate::rules::relay;
use b2bua_sdk::event::CallEvent;

/// Headers sent as top-level decision-request fields (excluded from
/// `sip_headers`). Shared with the failure path: `route-failure` applies the
/// same exclusion to the failed final response's headers, so "the
/// non-structural remainder" means one thing across the decision seam.
///
/// `Max-Forwards` is deliberately NOT here: a decision that cannot see the hop
/// count cannot answer a spent one. It rides `sip_headers` read-only — the stack
/// owns what a leg it originates states ([`crate::rules::relay::build_b_leg`]).
pub const STANDARD_HEADERS: &[HeaderName] = &[
    HeaderName::From,
    HeaderName::To,
    HeaderName::Via,
    HeaderName::Contact,
    HeaderName::ContentType,
    HeaderName::CallId,
    HeaderName::CSeq,
    HeaderName::ContentLength,
];

/// Build the initial [`Call`] (a-leg only) from an inbound INVITE, its
/// incarnation mark and its caller-facing tag seed drawn from `ids`. Pure.
pub fn build_initial_call(
    invite: &SipRequest,
    src: SocketAddr,
    config: &B2buaConfig,
    ids: &mut TurnIds,
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
    let limiter = CallLimiterState::uncounted(call::derive_incarnation(&call_ref, &ids.new_tag()));
    let tag_seed = ids.draw();
    let (local_addr, remote_addr) = relay::uas_addresses(invite);
    let a_leg = Leg {
        leg_id: "a".to_string(),
        call_id: invite.call_id().to_string(),
        source: RemoteInfo { address: src.ip().to_string(), port: src.port() },
        dialogs: Default::default(),
        local_uri: Some(local_addr),
        remote_uri: Some(remote_addr),
        invite_request_uri: Some(invite.request_uri().to_string()),
        invite: Some(call::RequestRecord::received_invite(i64::from(invite.cseq().seq()))),
        own_tag: None,
        ext: None,
        kind: Some(LegKind::A),
        // Derived from kind (the a-leg is always adopted); see `is_adopted`.
        adopted: None,
        attempt: None,
        ring: call::Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: Default::default(),
    };
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
        tag_seed,
    }
}

/// What the initial consult's answer leaves.
pub enum Initial {
    /// A route, to apply ([`crate::decision::route_fold`]).
    Route { call: Box<Call>, route: Box<RouteDecision> },
    /// The call's final.
    Final(Box<HandlerResult>),
}

/// The treatment the initial consult's `response` states for `call`, whose
/// caller's INVITE is `a_invite`: a route to apply, or the call's final.
pub fn treatment(
    call: Call,
    response: Result<NewCallResponse, CallDecisionError>,
    a_invite: &SipRequest,
    config: &B2buaConfig,
    ids: &mut TurnIds,
    now_ms: i64,
) -> Initial {
    let advertisement = &config.minted_final_advertisement;
    let result = match response {
        Ok(NewCallResponse::Route(route)) => {
            return Initial::Route { call: Box::new(call), route: Box::new(route) }
        }
        Ok(NewCallResponse::Reject(reject)) => apply_reject(
            call,
            reject,
            DecisionKind::Reject,
            Some("a".into()),
            a_invite,
            advertisement,
            ids,
            now_ms,
        ),
        Ok(NewCallResponse::Redirect(rd)) => apply_redirect(
            call,
            rd,
            DecisionKind::Redirect,
            Some("a".into()),
            a_invite,
            advertisement,
            ids,
            now_ms,
        ),
        // `Relay` is a failover-only treatment; with no captured downstream
        // failure at new-call time it falls back to 480 (ADR-0017 X5).
        Ok(NewCallResponse::Relay { label }) => {
            let call = mark_decision(call, now_ms, DecisionKind::Relay, Some("a".into()), label);
            reject_call(
                call,
                a_invite,
                480,
                Some("Temporarily Unavailable".into()),
                None,
                &[],
                advertisement,
                ids,
                now_ms,
                TerminationCause::DecisionReject,
            )
        }
        // No decision was returned: nothing to record, the stack's own final —
        // the one the engine stated, else 503.
        Err(CallDecisionError::Refused { code, reason, update_headers }) => {
            crate::decision::apply_reject::stated_refusal(
                call,
                code,
                reason,
                update_headers.as_ref(),
                a_invite,
                advertisement,
                ids,
                now_ms,
            )
        }
        Err(CallDecisionError::Unavailable(_)) => reject_call(
            call,
            a_invite,
            503,
            Some("Service Unavailable".into()),
            None,
            &[],
            advertisement,
            ids,
            now_ms,
            TerminationCause::Admission,
        ),
    };
    Initial::Final(Box::new(result))
}

/// Synthesize the setup `CallEvent` services' init actions resolve against (the
/// original a-leg INVITE on the a-leg). The `src` is the caller's address from
/// the a-leg, defaulting to `0.0.0.0:0` if it is not an `ip:port` (init actions
/// resolve legs from the call, not the event source).
pub fn setup_event(call: &Call, a_invite: &SipRequest) -> CallEvent {
    let src =
        sip_message::ip_literal::ip_socket_addr(&call.a_leg.source.address, call.a_leg.source.port)
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)));
    CallEvent::sip(SipMessage::Request(a_invite.clone()), src)
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
/// statements. Stated text the wire cannot carry is left out where every
/// authored final is built ([`relay::response_to_a_leg`]); an add left out
/// here is reported the same way.
#[allow(clippy::too_many_arguments)]
pub fn reject_call(
    mut call: Call,
    a_invite: &SipRequest,
    status: u16,
    reason: Option<String>,
    update_headers: Option<&SipHeaderUpdates>,
    contacts: &[RedirectContact],
    advertisement: &CapabilitySet,
    ids: &mut TurnIds,
    now_ms: i64,
    cause: TerminationCause,
) -> HandlerResult {
    let mut skipped = Vec::new();
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
                ids,
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
    // A non-100 final response needs a To-tag: the a-leg's own, as derived for
    // the call, so a provisional an earlier owner showed carried the same one.
    let to_tag = call
        .a_leg
        .own_tag
        .clone()
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| crate::caller_tags::own(&call));
    if let Some(effect) = relay::response_to_a_leg(
        &mut call,
        &mut effects,
        a_invite,
        status,
        &reason,
        Some(to_tag),
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
            *resp =
                crate::rules::stated_headers::response_with_adds(resp.clone(), &adds, &mut skipped);
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
    effects.buffered.extend(BufferedObservabilityEffect::skipped("rejecting final", skipped));
    call = record_termination(call, now_ms, cause, None);
    call.state = CallModelState::Terminated;
    HandlerResult { call, effects }
}

/// The decision request the initial consult sends for `invite`.
pub fn build_request(invite: &SipRequest) -> NewCallRequest {
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

#[cfg(test)]
mod emergency_on_invite_tests;

#[cfg(test)]
mod multi_instance_header_tests;
