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
use sip_message::header::{HeaderName, HeaderValue, Minted, Unsupported};
use sip_message::write_check::WriteRefusal;
use sip_message::{SipHeader, SipMessage, SipRequest, SipStr};

use crate::config::B2buaConfig;
use b2bua_sdk::consult_headers::consult_headers;
use b2bua_sdk::reason_phrase::default_reason;

use crate::decision::apply_reject::{apply_redirect, apply_reject};
use crate::decision::{
    CallDecisionError, HeaderUpdate, NewCallRequest, NewCallResponse, RedirectContact,
    RouteDecision, SipHeaderUpdates,
};
use crate::effects::{BufferedObservabilityEffect, HandlerEffects, HandlerResult};
use crate::rules::relay;
use b2bua_sdk::event::CallEvent;

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
    let emergency = is_emergency_request(invite).then_some(true);
    let contact = relay::allocate_leg_contact(
        config,
        relay::ContactMarks {
            call_ref: &call_ref,
            incarnation_mark: call::incarnation_mark(limiter.key()),
            is_emergency: emergency == Some(true),
        },
        "a",
    );
    let a_leg = Leg {
        leg_id: "a".to_string(),
        call_id: invite.call_id().to_string(),
        source: RemoteInfo { address: src.ip().to_string(), port: src.port() },
        dialogs: Default::default(),
        local_uri: Some(local_addr),
        remote_uri: Some(remote_addr),
        contact,
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
        emergency,
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
        setup_cancelled: false,
        termination: None,
        message_turn: 1,
        sm_cursors: std::collections::BTreeMap::new(),
        tag_seed,
        leg_space: call::LegSpace::Creator,
        answered: Vec::new(),
        awaited: Vec::new(),
        snoozes: Vec::new(),
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
    mut call: Call,
    response: Result<NewCallResponse, CallDecisionError>,
    a_invite: &SipRequest,
    config: &B2buaConfig,
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
            now_ms,
        ),
        Ok(NewCallResponse::Redirect(rd)) => apply_redirect(
            call,
            rd,
            DecisionKind::Redirect,
            Some("a".into()),
            a_invite,
            advertisement,
            now_ms,
        ),
        // `Relay` is a failover-only treatment; with no captured downstream
        // failure at new-call time it falls back to 480 (ADR-0017 X5).
        Ok(NewCallResponse::Relay { label }) => {
            mark_decision(&mut call, now_ms, DecisionKind::Relay, Some("a".into()), label);
            reject_call(
                call,
                a_invite,
                480,
                Some("Temporarily Unavailable".into()),
                None,
                &[],
                advertisement,
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
/// `Reason:`, RFC 3326); `contacts` renders one `Contact: <uri>;q=…;params…`
/// header per entry (used for a 3xx redirect — ADR-0017). A statement, set,
/// removal or add, of a header the final does not take from the decision, a
/// structural one or one the stack binds, is left out and reported
/// ([`HeaderName::stated_at_mint`]); an add yields to a line the final
/// carries, the advertisement's included. Marks no decision: the caller marks
/// the one it applies, and a final of the stack's own is none. `cause` is the termination record's: the decision layer's
/// refusal, or the stack's own admission. A failure final carries the
/// deployment's `advertisement` for a minted final under the decision's
/// statements. Stated text the wire cannot carry is left out where every
/// authored final is built ([`relay::response_to_a_leg`]); an add left out
/// here is reported the same way.
#[allow(clippy::too_many_arguments)]
pub fn reject_call(
    call: Call,
    a_invite: &SipRequest,
    status: u16,
    reason: Option<String>,
    update_headers: Option<&SipHeaderUpdates>,
    contacts: &[RedirectContact],
    advertisement: &CapabilitySet,
    now_ms: i64,
    cause: TerminationCause,
) -> HandlerResult {
    let mut skipped = Vec::new();
    let mut extra_headers = end_to_end_headers(update_headers, status, &mut skipped);
    // A redirect whose target does not read cannot be authored: the caller dials
    // what a 3xx Contact names, so an invented one sends it at an address the
    // decision never stated. Answer the plain server error instead, with no
    // Contact.
    match contact_headers(contacts) {
        Ok(headers) => {
            extra_headers.extend(headers);
            let stated = Stated { update_headers, extra_headers, advertisement, skipped };
            reject_with(call, a_invite, status, reason, stated, now_ms, cause)
        }
        Err(err) => {
            tracing::warn!(
                call_ref = %call.call_ref,
                detail = %err.detail(),
                "redirect refused"
            );
            let stated = Stated { update_headers, extra_headers, advertisement, skipped };
            let reason = Some(err.to_string());
            reject_with(call, a_invite, 500, reason, stated, now_ms, TerminationCause::Admission)
        }
    }
}

/// The stack's refusal of the caller's INVITE `a_invite` on `call` before any
/// decision is asked, or `None` when the INVITE is fit to route. One with no
/// From tag draws 400 (RFC 3261 §8.1.1.3): that tag is the caller dialog's
/// remote tag, which every request of the stack's in her dialog names. One
/// requiring an option tag this stack does not understand draws 420
/// ([`refuse_extension`]). Nobody is dialled; the call is born and ended, so
/// the reaper takes it and its delete propagates, as for a decision's reject.
pub fn refused_on_sight(
    call: &Call,
    a_invite: &SipRequest,
    advertisement: &CapabilitySet,
    now_ms: i64,
) -> Option<HandlerResult> {
    if call.a_leg_invite.from_tag.is_empty() {
        let reason = Some("Bad Request - missing From tag".into());
        let cause = TerminationCause::Admission;
        let call = call.clone();
        return Some(reject_call(
            call,
            a_invite,
            400,
            reason,
            None,
            &[],
            advertisement,
            now_ms,
            cause,
        ));
    }
    let unsupported = sip_message::option_tags::not_understood(a_invite)?;
    Some(refuse_extension(call.clone(), a_invite, &unsupported, advertisement, now_ms))
}

/// `call` refused 420 Bad Extension, its `Unsupported` listing `unsupported`,
/// the option tags the caller's INVITE `a_invite` requires and this call
/// cannot serve (RFC 3261 §8.2.2.3); the call ends with cause `Admission`.
pub fn refuse_extension(
    call: Call,
    a_invite: &SipRequest,
    unsupported: &Unsupported,
    advertisement: &CapabilitySet,
    now_ms: i64,
) -> HandlerResult {
    let name = HeaderName::Unsupported.as_wire_str().to_string();
    let stated: SipHeaderUpdates =
        [(name, HeaderUpdate::line(unsupported.to_wire()))].into_iter().collect();
    let cause = TerminationCause::Admission;
    reject_call(call, a_invite, 420, None, Some(&stated), &[], advertisement, now_ms, cause)
}

/// The headers a rejecting final carries: what the decision stated, the
/// headers built from it, the deployment's advertisement, and the statements
/// already left out.
struct Stated<'a> {
    update_headers: Option<&'a SipHeaderUpdates>,
    extra_headers: Vec<SipHeader>,
    advertisement: &'a CapabilitySet,
    skipped: Vec<WriteRefusal>,
}

/// The final `status` to the caller under `stated`, and the call ended
/// ([`reject_call`]).
fn reject_with(
    mut call: Call,
    a_invite: &SipRequest,
    status: u16,
    reason: Option<String>,
    stated: Stated<'_>,
    now_ms: i64,
    cause: TerminationCause,
) -> HandlerResult {
    let Stated { update_headers, mut extra_headers, advertisement, mut skipped } = stated;
    let reason = reason.unwrap_or_else(|| default_reason(status).to_string());
    // A failure final refused in this element's own name carries the
    // deployment's advertisement, each half under the decision's statements.
    if crate::rules::capabilities::is_minted_failure(status) {
        crate::rules::capabilities::stamp_own_advertisement(
            advertisement,
            |name| update_headers.is_some_and(|u| restates(u, name)),
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
        add_cdr_event(
            &mut call,
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
    record_termination(&mut call, now_ms, cause, None);
    call.state = CallModelState::Terminated;
    HandlerResult { call, effects }
}

/// The decision request the initial consult sends for `invite`.
pub fn build_request(invite: &SipRequest) -> NewCallRequest {
    let sip_headers = consult_headers(invite.headers());
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

/// Whether `updates` set or remove the header `name` names: an add of it
/// yields to the line the final carries, the advertisement's included.
fn restates(updates: &SipHeaderUpdates, name: &HeaderName) -> bool {
    updates.iter().any(|(n, update)| !update.adds() && name.matches(n))
}

/// The extra response headers a reject/redirect of `status` takes from
/// `update_headers`: the *sets* (e.g. `Reason:`) of a header the final takes
/// from the decision ([`HeaderName::stated_at_mint`]), one line per stated
/// line — its adds yield to the final as built ([`reject_call`]), and a
/// removal finds nothing to remove. A set or removal of the stack's own
/// header, the Contact a redirect authors from its typed target list included
/// (ADR-0017 X2), or of one the transaction, a negotiation or the body binds,
/// is refused onto `skipped`.
fn end_to_end_headers(
    update_headers: Option<&SipHeaderUpdates>,
    status: u16,
    skipped: &mut Vec<WriteRefusal>,
) -> Vec<SipHeader> {
    let mut out: Vec<SipHeader> = Vec::new();
    let sets = update_headers.into_iter().flatten().filter(|(_, val)| !val.adds());
    for (name, val) in sets {
        if let Err(refusal) = HeaderName::stated_at_mint(name, Minted::Final(status)) {
            skipped.push(refusal);
            continue;
        }
        for v in val.lines() {
            out.push(SipHeader { name: SipStr::owned(name), value: SipStr::owned(v) });
        }
    }
    out
}

/// One `Contact: <uri>;q=…;params…` per redirect target, or the error of the
/// first that does not read: the whole redirect is refused, never partially
/// authored (dropping the unreadable entry would send the caller at whichever
/// targets happened to parse).
fn contact_headers(
    contacts: &[RedirectContact],
) -> Result<Vec<SipHeader>, relay::UnreadableAddress> {
    contacts.iter().map(relay::redirect_contact).collect()
}

#[cfg(test)]
mod emergency_on_invite_tests;

#[cfg(test)]
mod multi_instance_header_tests;
