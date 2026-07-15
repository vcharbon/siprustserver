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
use sip_message::message_helpers::{
    get_header, get_headers, is_emergency_request, parse_uri_params,
};
use sip_message::{SipMessage, SipRequest};
use sip_txn::IdGen;

use crate::config::B2buaConfig;
use crate::decision::apply_route::apply_route;
use crate::decision::{
    CallDecisionEngine, NewCallRequest, NewCallResponse, RedirectContact, SipHeaderUpdates,
};
use crate::effects::{HandlerEffects, HandlerResult};
use crate::event::CallEvent;
use crate::limiter::CallLimiter;
use crate::rules::{relay, seed_services, ActionExecutor, ServiceDef};

/// Headers sent as top-level decision-request fields (excluded from
/// `sip_headers`). Shared with the failure path: `route-failure` applies the
/// same exclusion to the failed final response's headers, so "the
/// non-structural remainder" means one thing across the decision seam.
pub(crate) const STANDARD_HEADERS: &[&str] = &[
    "from", "to", "via", "contact", "content-type", "call-id", "cseq", "max-forwards",
    "content-length",
];

/// Derive the HA [`CallTopology`] from the proxy's stickiness cookie, carried as
/// URI params on the front proxy's Record-Route (`w_pri`/`w_bak`/`e`/`v`/`kid`/
/// `sig`; see `sip-proxy` `load_balancer.rs` / `build_record_route_value`). The
/// b2bua reads `w_pri`/`w_bak` so the backup peer AGREES with the proxy's
/// rendezvous (HRW 2nd-best) choice by construction rather than re-deriving it
/// (the proxy keys HRW off the alive-set + Call-ID the b2bua cannot reproduce
/// locally — see [`crate::repl::replication_target`]).
///
/// The proxy double-record-routes (`ProxyCore` §16.6): it inserts BOTH a cookie
/// RR and a direction-only `;outbound` RR, and on an inbound INVITE the
/// `;outbound` half is topmost. So we scan all Record-Route headers for the one
/// that actually carries the stickiness grammar rather than assuming the first.
///
/// - Cookie present → `Some(CallTopology { pri: w_pri (or self_ordinal if the
///   param is absent/empty), bak: w_bak (may be empty), gen: 1 })`. A brand-new
///   call starts at `gen = 1`; the b2bua's update path bumps it per mutation (see
///   [`crate::store::CallState::update`]).
/// - No cookie (non-proxied / legacy INVITE) → `None`: the flush path then stays
///   non-replicating (`PutOpts::default()`), preserving today's behaviour.
fn topology_from_cookie(invite: &SipRequest, self_ordinal: &str) -> Option<CallTopology> {
    // Find the Record-Route that carries the stickiness cookie; the partner
    // `;outbound` half (direction only) has no `w_pri`/`w_bak`.
    let params = get_headers(&invite.headers, "record-route")
        .into_iter()
        .map(parse_uri_params)
        .find(|p| p.contains_key("w_pri") || p.contains_key("w_bak"))?;
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
        // Derived from kind (the a-leg is always adopted); see `is_adopted`.
        adopted: None,
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
        // A non-emergency INVITE writes *absence* (`None`), never `Some(false)`
        // — production only ever stamps `Some(true)` or leaves the field unset
        // (see `call` codec_roundtrip emergency contract). Downstream consumers
        // — the `;emerg=1`/`;em=1` URI/Via markers in `stack_identity` and the
        // cheap in-dialog byte classifier (`buffer_has_emergency_marker`) —
        // depend on this being derived from the INVITE here, not hard-coded.
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
        sm_cursors: std::collections::BTreeMap::new(),
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
    services: &[ServiceDef],
    now_ms: i64,
) -> HandlerResult {
    let a_invite = relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let req = build_request(&a_invite);

    match decision.new_call(req).await {
        Ok(NewCallResponse::Route(route)) => {
            // The route built the Call; now run each service's `init` (ADR-0016
            // X8) — the source's `call-routed` re-entry point — folding any seed
            // (cursor + data + initial actions) through the normal executor.
            // A dormant service (`init` → `None`) and the empty-service list
            // (production today) both leave the result untouched.
            let result =
                apply_route(call, route, &a_invite, decision, limiter, config, id_gen, now_ms, 0)
                    .await;
            let exec = ActionExecutor { config, id_gen, now_ms };
            let setup_event = setup_event(&result.call, &a_invite);
            seed_services(result, services, &exec, &setup_event, "a", call::Direction::FromA)
        }
        Ok(NewCallResponse::Reject(reject)) => reject_call(
            call,
            &a_invite,
            reject.reject_code,
            reject.reject_reason,
            reject.update_headers.as_ref(),
            &[],
            id_gen,
            now_ms,
        ),
        Ok(NewCallResponse::Redirect(rd)) => reject_call(
            call,
            &a_invite,
            rd.code,
            rd.reason,
            rd.update_headers.as_ref(),
            &rd.contacts,
            id_gen,
            now_ms,
        ),
        // `Relay` is a failover-only treatment; with no captured downstream
        // failure at new-call time it falls back to 480 (ADR-0017 X5).
        Ok(NewCallResponse::Relay) => reject_call(
            call,
            &a_invite,
            480,
            Some("Temporarily Unavailable".into()),
            None,
            &[],
            id_gen,
            now_ms,
        ),
        Err(_unavailable) => {
            reject_call(call, &a_invite, 503, Some("Service Unavailable".into()), None, &[], id_gen, now_ms)
        }
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
    }
}

/// Answer the a-leg with a final failure / redirect the decision layer authors.
/// `update_headers` adds non-structural headers (e.g. `Reason:`, RFC 3326);
/// `contacts` renders one `Contact: <uri>;q=…` header per entry (used for a 3xx
/// redirect — ADR-0017). Structural headers are skipped (the generator owns
/// them, header-ownership matrix X2).
#[allow(clippy::too_many_arguments)]
pub(crate) fn reject_call(
    mut call: Call,
    a_invite: &SipRequest,
    status: u16,
    reason: Option<String>,
    update_headers: Option<&SipHeaderUpdates>,
    contacts: &[RedirectContact],
    id_gen: &IdGen,
    now_ms: i64,
) -> HandlerResult {
    let reason = reason.unwrap_or_else(|| default_reason(status));
    let extra_headers = build_reject_headers(update_headers, contacts);
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
        extra_headers,
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
    let mut sip_headers: std::collections::BTreeMap<String, Vec<String>> =
        std::collections::BTreeMap::new();
    for h in &invite.headers {
        if STANDARD_HEADERS.contains(&h.name.to_ascii_lowercase().as_str()) {
            continue;
        }
        sip_headers.entry(h.name.clone()).or_default().push(h.value.clone());
    }
    NewCallRequest {
        call_id: invite.call_id.clone(),
        ruri: invite.uri.clone(),
        from: get_header(&invite.headers, "from").unwrap_or("").to_string(),
        to: get_header(&invite.headers, "to").unwrap_or("").to_string(),
        via: get_headers(&invite.headers, "via").iter().map(|s| s.to_string()).collect(),
        contact: get_headers(&invite.headers, "contact").iter().map(|s| s.to_string()).collect(),
        content_type: get_header(&invite.headers, "content-type").map(str::to_string),
        sip_headers,
        sip_body: (!invite.body.is_empty()).then(|| String::from_utf8_lossy(&invite.body).into_owned()),
    }
}

fn default_reason(status: u16) -> String {
    match status {
        302 => "Moved Temporarily",
        403 => "Forbidden",
        404 => "Not Found",
        480 => "Temporarily Unavailable",
        486 => "Busy Here",
        503 => "Service Unavailable",
        _ => "Declined",
    }
    .to_string()
}

/// Structural headers the response generator owns — never settable via the flat
/// header map (ADR-0017 X2). `Contact` is excluded too: on a redirect it is
/// authored from the typed `contacts` list, elsewhere the B2BUA owns it.
const REJECT_STRUCTURAL_HEADERS: &[&str] = &[
    "from", "to", "via", "call-id", "cseq", "max-forwards", "content-length", "record-route",
    "contact",
];

/// Build the extra response headers for a reject/redirect: the non-structural
/// `update_headers` *sets* (e.g. `Reason:`) plus one `Contact: <uri>;q=…` per
/// redirect target. Removals and structural keys are dropped.
fn build_reject_headers(
    update_headers: Option<&SipHeaderUpdates>,
    contacts: &[RedirectContact],
) -> Vec<sip_message::SipHeader> {
    let mut out: Vec<sip_message::SipHeader> = Vec::new();
    if let Some(map) = update_headers {
        for (name, val) in map {
            let is_structural = REJECT_STRUCTURAL_HEADERS
                .contains(&name.to_ascii_lowercase().as_str());
            if let (Some(v), false) = (val, is_structural) {
                out.push(sip_message::SipHeader { name: name.clone(), value: v.clone() });
            }
        }
    }
    for c in contacts {
        let value = match c.q {
            Some(q) => format!("<{}>;q={q}", c.uri),
            None => format!("<{}>", c.uri),
        };
        out.push(sip_message::SipHeader { name: "Contact".to_string(), value });
    }
    out
}

#[cfg(test)]
mod emergency_on_invite_tests {
    //! Pins that [`build_initial_call`] stamps `Call.emergency` from the
    //! inbound INVITE: `Some(true)` for an emergency Resource-Priority,
    //! *absence* (`None`) otherwise — never `Some(false)`. Every downstream
    //! emergency consumer (the `;emerg=1`/`;em=1` markers, the in-dialog byte
    //! classifier) depends on this seam being live.
    //!
    //! These assert the *wiring* (helper → field + the None coercion), not the
    //! emergency-classification contract itself — that lives in
    //! `sip_message::message_helpers::emergency`. Pure builder, no clock.

    use super::build_initial_call;
    use crate::config::B2buaConfig;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser, SipRequest};
    use std::net::SocketAddr;

    fn config_for(ordinal: &str) -> B2buaConfig {
        B2buaConfig { self_ordinal: ordinal.into(), ..Default::default() }
    }

    fn src() -> SocketAddr {
        SocketAddr::from(([10, 0, 0, 9], 5060))
    }

    /// Parse a minimal INVITE; `rph` (when `Some`) is the `Resource-Priority`
    /// header VALUE (header name canonical). `None` omits the header entirely.
    fn invite_with_rph(rph: Option<&str>) -> SipRequest {
        let rph_line = match rph {
            Some(v) => format!("Resource-Priority: {v}\r\n"),
            None => String::new(),
        };
        let raw = format!(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-emerg-iih\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: emerg-iih@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             {rph_line}\
             Content-Length: 0\r\n\r\n"
        );
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    #[test]
    fn emergency_invite_sets_emergency_true() {
        // Each canonical emergency RPH token flags the new a-leg Call as emergency.
        for tok in ["esnet.0", "wps.0", "q735.0"] {
            let invite = invite_with_rph(Some(tok));
            let call = build_initial_call(&invite, src(), &config_for("w0"), 0);
            assert_eq!(
                call.emergency,
                Some(true),
                "RPH {tok} must stamp emergency = Some(true) on the a-leg Call"
            );
        }
    }

    #[test]
    fn non_emergency_invite_leaves_emergency_unset() {
        // No Resource-Priority at all → field stays absent (`None`), NOT Some(false).
        let call = build_initial_call(&invite_with_rph(None), src(), &config_for("w0"), 0);
        assert_eq!(call.emergency, None, "no RPH → emergency stays None");

        // A well-formed but non-emergency RPH namespace.value also stays None.
        let call = build_initial_call(&invite_with_rph(Some("dsn.flash")), src(), &config_for("w0"), 0);
        assert_eq!(call.emergency, None, "non-emergency RPH → emergency stays None");
    }

    #[test]
    fn emergency_is_derived_through_the_real_helper() {
        // Proves the field is wired to `is_emergency_request` and not a naive
        // header-presence check: r-values compare case-insensitively (RFC 4412)…
        let call = build_initial_call(&invite_with_rph(Some("ESNET.0")), src(), &config_for("w0"), 0);
        assert_eq!(call.emergency, Some(true), "r-value casing must not gate emergency");

        // …an emergency r-value among multiple namespaces flags…
        let call =
            build_initial_call(&invite_with_rph(Some("dsn.flash, q735.0")), src(), &config_for("w0"), 0);
        assert_eq!(call.emergency, Some(true), "emergency r-value in a list flags emergency");

        // …and an r-value merely embedding a token does not.
        let call = build_initial_call(&invite_with_rph(Some("esnet.01")), src(), &config_for("w0"), 0);
        assert_eq!(call.emergency, None, "embedded token is not an emergency r-value");
    }
}

#[cfg(test)]
mod multi_instance_header_tests {
    //! Repeatable SIP headers (`History-Info`, `Diversion`, `P-Asserted-Identity`,
    //! `Contact`) spread across separate lines must reach the decision engine
    //! intact — collapsing to the last line silently drops every earlier hop, and
    //! nothing downstream can restore a value lost above the adapter.

    use super::build_request;
    use sip_message::parser::custom::CustomParser;
    use sip_message::{SipMessage, SipParser, SipRequest};

    fn parse(raw: &str) -> SipRequest {
        match CustomParser::new().parse(raw.as_bytes()).expect("fixture INVITE should parse") {
            SipMessage::Request(r) => r,
            SipMessage::Response(_) => panic!("expected a request"),
        }
    }

    #[test]
    fn repeated_header_lines_and_contacts_survive_in_wire_order() {
        let invite = parse(
            "INVITE sip:bob@example.com SIP/2.0\r\n\
             Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-multi\r\n\
             Max-Forwards: 70\r\n\
             From: <sip:alice@example.com>;tag=alicetag\r\n\
             To: <sip:bob@example.com>\r\n\
             Call-ID: multi@10.0.0.9\r\n\
             CSeq: 1 INVITE\r\n\
             Contact: <sip:alice@10.0.0.9:5060>\r\n\
             History-Info: <sip:a@ex.com>;index=1\r\n\
             History-Info: <sip:b@ex.com>;index=1.1\r\n\
             History-Info: <sip:c@ex.com>;index=1.1.1\r\n\
             Diversion: <sip:d1@ex.com>;reason=user-busy\r\n\
             Diversion: <sip:d2@ex.com>;reason=no-answer\r\n\
             Content-Length: 0\r\n\r\n",
        );
        let req = build_request(&invite);

        // Contact is populated via the plural getter, so any number of instances
        // survives (the parser caps an INVITE at one per RFC 3261 §8.1.1.8, but the
        // schema no longer collapses — it carries whatever arrived).
        assert_eq!(req.contact, vec!["<sip:alice@10.0.0.9:5060>".to_string()]);

        // Every hop of a multi-line header survives, in wire order.
        assert_eq!(
            req.sip_headers.get("History-Info").map(Vec::as_slice),
            Some(
                &[
                    "<sip:a@ex.com>;index=1".to_string(),
                    "<sip:b@ex.com>;index=1.1".to_string(),
                    "<sip:c@ex.com>;index=1.1.1".to_string(),
                ][..]
            ),
        );
        assert_eq!(req.sip_headers.get("Diversion").map(Vec::len), Some(2));

        // The single-value accessor returns the first instance for the common read.
        assert_eq!(req.sip_header("History-Info"), Some("<sip:a@ex.com>;index=1"));
        assert_eq!(req.sip_header("Absent"), None);
    }
}
