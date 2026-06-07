//! Rule-engine unit tests: matcher ranking + overrides, invariant enforcement,
//! and a representative default-rule's action output — pinned at the rule seam
//! without a full SUT.

use std::net::SocketAddr;

use b2bua::config::B2buaConfig;
use b2bua::effects::{BufferedObservabilityEffect, CriticalStateEffect, HandlerResult};
use b2bua::event::CallEvent;
use b2bua::initial_invite::build_initial_call;
use b2bua::rules::{
    default_rules, execute_rules, invariants, pick_ranked, ActionExecutor, Match, RuleAction,
    RuleContext, RuleDefinition, RuleHandleResult, SERVICE_LAYER,
};
use call::{
    B2buaDialogExt, CallModelState, Dialog, Direction, Leg, LegDisposition, LegKind, LegState,
    MachineId, RemoteInfo, StackDialog, StateLabel, TimerType,
};
use sip_txn::IdGen;
use sip_message::generators::{
    generate_out_of_dialog_request, ContactSpec, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
    SipTransport, ViaSpec,
};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser, SipRequest};

fn invite() -> SipRequest {
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: "sip:bob@127.0.0.1:5070".into(),
        call_id: "c1@alice".into(),
        from_uri: "sip:alice@host".into(),
        from_tag: "atag".into(),
        to_uri: "sip:bob@host".into(),
        to_tag: None,
        cseq: 1,
        via: Some(ViaSpec {
            local_ip: "127.0.0.1".into(),
            local_port: 5060,
            transport: SipTransport::Udp,
            branch: "z9hG4bKalice".into(),
            custom_params: vec![],
        }),
        contact: Some(ContactSpec {
            user: "alice".into(),
            host: "127.0.0.1".into(),
            port: 5060,
            uri_params: vec![],
        }),
        max_forwards: Some(70),
        body: b"v=0\r\n".to_vec(),
        content_type: None,
        extra_headers: vec![],
    };
    generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts)
}

fn test_call() -> call::Call {
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    build_initial_call(&invite(), src, &B2buaConfig::default(), 0)
}

#[test]
fn timer_global_duration_selects_max_duration() {
    let call = test_call();
    let event = CallEvent::Timer {
        timer_type: TimerType::GlobalDuration,
        call_ref: call.call_ref.clone(),
        leg_id: None,
    };
    let ctx = RuleContext {
        call: &call,
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &B2buaConfig::default(),
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("max-duration"));
}

#[test]
fn in_dialog_bye_selects_relay_bye() {
    let call = test_call();
    // An in-dialog BYE (carries a To-tag) on the active call.
    let mut bye = invite();
    bye.method = "BYE".into();
    bye.to.tag = Some("btag".into());
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(bye)),
        src: "127.0.0.1:5060".parse().unwrap(),
    };
    let ctx = RuleContext {
        call: &call,
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &B2buaConfig::default(),
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("relay-bye"));
}

#[test]
fn invariants_append_cleanup_on_terminated() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    call.a_leg.state = LegState::Terminated;
    let result = invariants::enforce(&before, HandlerResult::new(call));

    assert!(
        result.effects.critical.iter().any(|e| matches!(e, CriticalStateEffect::CancelAllTimers)),
        "cancel-all-timers guaranteed"
    );
    assert!(
        result.effects.buffered.iter().any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr)),
        "write-cdr guaranteed"
    );
    assert!(
        matches!(result.effects.critical.last(), Some(CriticalStateEffect::RemoveCall)),
        "remove-call runs last"
    );
}

// ── ADR-0016 slice 2: global call machine projection ────────────────────────

#[test]
fn global_call_cursor_projects_lifecycle_states() {
    let cursor = |r: &HandlerResult| {
        r.call
            .sm_cursors
            .get(&invariants::GLOBAL_CALL_MACHINE)
            .map(StateLabel::as_str)
            .map(str::to_string)
    };
    let mut call = test_call();

    // Active call → "Active".
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(cursor(&r).as_deref(), Some("Active"));

    // Terminating with a still-confirmed a-leg (unresolved → no promotion)
    // → "Terminating".
    call.state = CallModelState::Terminating;
    call.a_leg.state = LegState::Confirmed;
    call.a_leg.bye_disposition = None;
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(r.call.state, CallModelState::Terminating, "not promoted while unresolved");
    assert_eq!(cursor(&r).as_deref(), Some("Terminating"));

    // Terminated → "Terminated".
    call.state = CallModelState::Terminated;
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(cursor(&r).as_deref(), Some("Terminated"));
}

// ── ADR-0016 slice 1: machine-gated selection + SetState + transition check ──

const TEST_MACHINE: &str = "test-machine";

static SM_ACTIVE_S0: [StateLabel; 1] = [StateLabel::new("S0")];
static SM_TRANSITIONS: [(StateLabel, StateLabel); 1] =
    [(StateLabel::new("S0"), StateLabel::new("S1"))];

fn handle_to_s1(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::SetState {
        machine: MachineId::new(TEST_MACHINE),
        to: StateLabel::new("S1"),
    }]))
}

/// Emits an undeclared S0 → S2 move (only S0 → S1 is in `SM_TRANSITIONS`).
fn handle_to_s2_undeclared(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::SetState {
        machine: MachineId::new(TEST_MACHINE),
        to: StateLabel::new("S2"),
    }]))
}

fn sm_rule(handle: fn(&RuleContext) -> Option<RuleHandleResult>) -> RuleDefinition {
    RuleDefinition {
        id: "test-sm-rule",
        layer: SERVICE_LAYER,
        overrides: &[],
        matcher: Match::request().method("INFO"),
        handle,
        machine: Some(MachineId::new(TEST_MACHINE)),
        active_states: &SM_ACTIVE_S0,
        transitions: &SM_TRANSITIONS,
    }
}

fn info_event() -> CallEvent {
    let mut info = invite();
    info.method = "INFO".into();
    info.to.tag = Some("btag".into());
    CallEvent::Sip {
        message: Box::new(SipMessage::Request(info)),
        src: "127.0.0.1:5060".parse().unwrap(),
    }
}

fn ctx_for<'a>(call: &'a call::Call, event: &'a CallEvent, config: &'a B2buaConfig) -> RuleContext<'a> {
    RuleContext {
        call,
        call_ref: &call.call_ref,
        event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config,
    }
}

#[test]
fn machine_rule_is_candidate_only_in_active_state() {
    let mut call = test_call();
    let event = info_event();
    let config = B2buaConfig::default();
    let rules = vec![sm_rule(handle_to_s1)];

    // No cursor seeded → machine dormant → not a candidate.
    {
        let ctx = ctx_for(&call, &event, &config);
        assert!(pick_ranked(&rules, &ctx).is_empty(), "dormant without a cursor");
    }

    // Cursor in `active_states` (S0) → candidate.
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    {
        let ctx = ctx_for(&call, &event, &config);
        assert_eq!(
            pick_ranked(&rules, &ctx).first().map(|r| r.id),
            Some("test-sm-rule"),
            "candidate when cursor ∈ active_states"
        );
    }

    // Cursor outside `active_states` (S1) → skipped.
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S1"));
    {
        let ctx = ctx_for(&call, &event, &config);
        assert!(pick_ranked(&rules, &ctx).is_empty(), "skipped when cursor ∉ active_states");
    }
}

#[test]
fn set_state_moves_cursor_and_gates_the_next_event() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let rules = vec![sm_rule(handle_to_s1)];

    let result = {
        let ctx = ctx_for(&call, &event, &config);
        execute_rules(&rules, &ctx, &exec, |c: &RuleContext| HandlerResult::new(c.call.clone()))
    };
    assert_eq!(
        result.call.sm_cursors.get(&MachineId::new(TEST_MACHINE)).map(StateLabel::as_str),
        Some("S1"),
        "SetState moved the cursor to S1"
    );

    // The next event sees the new state: the S0-gated rule no longer fires.
    let next = result.call;
    let ctx2 = ctx_for(&next, &event, &config);
    assert!(
        pick_ranked(&rules, &ctx2).is_empty(),
        "the S0-gated rule is no longer a candidate at S1"
    );
}

/// Only debug builds panic (release logs and proceeds), so gate the test on
/// `debug_assertions` — the suite runs in debug.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "undeclared transition")]
fn undeclared_transition_trips_debug_assert() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let rules = vec![sm_rule(handle_to_s2_undeclared)];

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &ctx, &exec, |c: &RuleContext| HandlerResult::new(c.call.clone()));
}

// ── b-leg route-set capture (RFC 3261 §12.1.2) ──────────────────────────────
//
// The b-leg is a UAC dialog, so `ConfirmDialog` must build its route set from
// the dialog-creating 2xx's Record-Route headers in *reverse* order. Without
// this the route set stays empty and every worker→callee in-dialog request
// (keepalive OPTIONS, BYE, re-INVITE) falls back to the synthetic
// `b2b_outbound_proxy` Route in `relay::apply_b_leg_egress`, silently dropping
// the proxy's signed Record-Route cookie. Regression guard: the transparent-
// failover matrix routes identically with or without this capture (the proxy
// classifies the request `;outbound` and ignores the cookie either way), so the
// only place the omission shows is the Route header content asserted here.

/// A pending (Trying) b-leg with one dialog whose route set is still empty —
/// the state `ConfirmDialog` mutates when the 2xx arrives.
fn b_leg_pending() -> Leg {
    let dialog = Dialog {
        sip: StackDialog {
            call_id: "bcid@x".into(),
            local_tag: "svc".into(),
            remote_tag: String::new(),
            local_uri: "sip:svc@10.0.0.9".into(),
            remote_uri: "sip:bob@10.0.0.2".into(),
            remote_target: "sip:bob@10.0.0.2:5070".into(),
            local_cseq: 1,
            route_set: vec![],
        },
        ext: B2buaDialogExt {
            remote_cseq: None,
            inbound_pending_requests: vec![],
            ack_branch: None,
            pending_invite_txn: None,
            cached_sdp: None,
        },
    };
    Leg {
        leg_id: "b-1".into(),
        call_id: "bcid@x".into(),
        from_tag: "svc".into(),
        source: RemoteInfo { address: "10.0.0.2".into(), port: 5070 },
        state: LegState::Trying,
        disposition: LegDisposition::Pending,
        dialogs: vec![dialog],
        no_answer_timeout_sec: None,
        bye_disposition: None,
        local_uri: Some("sip:svc@10.0.0.9".into()),
        remote_uri: Some("sip:bob@10.0.0.2".into()),
        invite_request_uri: Some("sip:bob@10.0.0.2:5070".into()),
        pending_invite_txn: None,
        ext: None,
        kind: Some(LegKind::Destination),
        adopted: None,
    }
}

#[test]
fn confirm_dialog_captures_b_leg_route_set_from_2xx_record_route_reversed() {
    let mut call = test_call();
    call = call::helpers::add_b_leg(call, b_leg_pending());

    // 2xx with two Record-Routes (top-to-bottom: proxy-b then proxy-a). §12.1.2:
    // the UAC route set is this list *reversed* → [proxy-a, proxy-b].
    let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: <sip:proxy-b.example:5060;lr>\r\n\
Record-Route: <sip:proxy-a.example:5060;lr>\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Length: 0\r\n\r\n";
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Response(resp)),
        src: "10.0.0.2:5070".parse().unwrap(),
    };
    let config = B2buaConfig::default();
    let ctx = RuleContext {
        call: &call,
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &ctx);

    let d = &result.call.b_legs[0].dialogs[0].sip;
    assert_eq!(
        d.route_set,
        vec![
            "<sip:proxy-a.example:5060;lr>".to_string(),
            "<sip:proxy-b.example:5060;lr>".to_string(),
        ],
        "b-leg route set must be the 2xx Record-Route in reverse order (§12.1.2)"
    );
    // The other 2xx-learned fields stay correct alongside the new capture.
    assert_eq!(d.remote_tag, "bobtag");
    assert_eq!(d.remote_target, "sip:bob@10.0.0.2:5070");
}

#[test]
fn confirm_dialog_without_record_route_leaves_route_set_empty() {
    // No Record-Route on the 2xx (single-hop, no record-routing proxy) → the
    // b-leg route set stays empty and in-dialog egress uses the remote target /
    // outbound-proxy fallback. Guards against clobbering with a bogus entry.
    let mut call = test_call();
    call = call::helpers::add_b_leg(call, b_leg_pending());

    let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:bob@10.0.0.2:5070>\r\n\
Content-Length: 0\r\n\r\n";
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Response(resp)),
        src: "10.0.0.2:5070".parse().unwrap(),
    };
    let config = B2buaConfig::default();
    let ctx = RuleContext {
        call: &call,
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &ctx);

    assert!(
        result.call.b_legs[0].dialogs[0].sip.route_set.is_empty(),
        "no Record-Route on the 2xx → b-leg route set stays empty"
    );
}

// ── Slice 5: media/INFO primitives + leg-kind relay gate (ADR-0016) ──────────
//
// Ports `tests/b2bua/leg-kind-gate.test.ts` (source pin
// fffc4ac6 — see MIGRATION_STATUS.md): an unadopted `media` leg is gated out of
// the generic relay-to-peer `→ a` fallback, while an adopted leg still relays;
// `send-provisional-to-leg` brokers a 183 early-media onto the a-leg; and
// `send-request-to-leg` carries an opaque MSCML INFO body to a named leg.
mod media_primitives {
    use super::*;
    use b2bua::effects::OutboundBody;
    use b2bua::rules::MessageTransform;
    use sip_message::message_helpers::get_header;
    use sip_message::SipHeader;

    /// A confirmed b-leg of the given role; its single dialog carries the callee
    /// tag so in-dialog originators have a confirmed dialog to ride.
    fn confirmed_b_leg(leg_id: &str, kind: LegKind) -> Leg {
        let mut leg = super::b_leg_pending();
        leg.leg_id = leg_id.into();
        leg.state = LegState::Confirmed;
        leg.kind = Some(kind);
        leg.adopted = None; // derive adoption from the kind
        leg.dialogs[0].sip.remote_tag = "bobtag".into();
        leg.dialogs[0].ext.remote_cseq = Some(1);
        leg
    }

    /// Give the a-leg a confirmed dialog so a relayed in-dialog request toward A
    /// has a target dialog to ride.
    fn give_a_leg_dialog(call: &mut call::Call) {
        call.a_leg.state = LegState::Confirmed;
        call.a_leg.dialogs = vec![Dialog {
            sip: StackDialog {
                call_id: call.a_leg.call_id.clone(),
                local_tag: "a-svc".into(),
                remote_tag: call.a_leg.from_tag.clone(),
                local_uri: "sip:svc@10.0.0.9".into(),
                remote_uri: "sip:alice@host".into(),
                remote_target: "sip:alice@127.0.0.1:5060".into(),
                local_cseq: 1,
                route_set: vec![],
            },
            ext: B2buaDialogExt {
                remote_cseq: Some(1),
                inbound_pending_requests: vec![],
                ack_branch: None,
                pending_invite_txn: None,
                cached_sdp: None,
            },
        }];
    }

    /// An in-dialog INFO request (carries a To-tag) with a DTMF payload.
    fn in_dialog_info() -> SipRequest {
        let mut info = super::invite();
        info.method = "INFO".into();
        info.to.tag = Some("svc".into());
        info.cseq.seq = 2;
        info.cseq.method = "INFO".into();
        info.body = b"Signal=5\r\nDuration=160\r\n".to_vec();
        info.headers.push(SipHeader {
            name: "Content-Type".into(),
            value: "application/dtmf-relay".into(),
        });
        info
    }

    fn exec_on<'a>(
        call: &'a call::Call,
        event: &'a CallEvent,
        source_leg_id: &'a str,
        config: &'a B2buaConfig,
        id_gen: &'a IdGen,
        actions: &[RuleAction],
    ) -> HandlerResult {
        let exec = ActionExecutor { config, id_gen, now_ms: 0 };
        let ctx = RuleContext {
            call,
            call_ref: &call.call_ref,
            event,
            source_leg_id,
            direction: Direction::FromB,
            now_ms: 0,
            config,
        };
        exec.execute(actions, &ctx)
    }

    // Port of leg-kind-gate test 1: relay-to-peer from an unadopted media leg
    // produces ZERO outbound — it must NOT fall back to A.
    #[test]
    fn relay_to_peer_is_gated_for_unadopted_media_leg() {
        let mut call = test_call();
        give_a_leg_dialog(&mut call);
        call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Media));
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
        };
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
        );
        assert!(
            result.effects.outbound.is_empty(),
            "an unadopted media leg's relay-to-peer must not be mis-routed to A"
        );
    }

    // Port of leg-kind-gate test 2: an adopted (destination) leg still falls back
    // to A — the gate only suppresses unadopted legs.
    #[test]
    fn relay_to_peer_falls_back_to_a_for_adopted_destination_leg() {
        let mut call = test_call();
        give_a_leg_dialog(&mut call);
        call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Destination));
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
        };
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
        );
        assert_eq!(result.effects.outbound.len(), 1, "adopted leg relays to its peer");
        assert_eq!(
            result.effects.outbound[0].leg_id.as_deref(),
            Some("a"),
            "the relay falls back to the a-leg"
        );
    }

    // INFO with an opaque MSCML body is emitted verbatim to the named leg with
    // the given content type (the MSCML control channel toward an MRF).
    #[test]
    fn send_request_to_leg_emits_info_with_mscml_body() {
        let mut call = test_call();
        call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Media));
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
        };
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let mscml = b"<MediaServerControl><request><play/></request></MediaServerControl>".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::SendRequestToLeg {
                leg_id: "b-1".into(),
                method: "INFO".into(),
                body: mscml.clone(),
                content_type: Some("application/mediaservercontrol+xml".into()),
            }],
        );
        assert_eq!(result.effects.outbound.len(), 1);
        let eff = &result.effects.outbound[0];
        assert_eq!(eff.leg_id.as_deref(), Some("b-1"));
        match &eff.body {
            OutboundBody::Request(r) => {
                assert_eq!(r.method, "INFO");
                assert_eq!(r.body, mscml, "MSCML body passes through opaquely");
                assert_eq!(
                    get_header(&r.headers, "content-type"),
                    Some("application/mediaservercontrol+xml")
                );
            }
            _ => panic!("expected an outbound request"),
        }
    }

    // 183 brokers an unadopted leg's SDP onto the a-leg as unreliable early media
    // (RFC 3262 §3 / RFC 5009 P-Early-Media), minting the B2BUA's a-facing tag.
    #[test]
    fn send_provisional_to_leg_brokers_183_sdp_to_a() {
        let call = test_call();
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
        };
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let sdp = b"v=0\r\no=mrf 1 1 IN IP4 10.0.0.50\r\n".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::SendProvisionalToLeg {
                leg_id: "a".into(),
                status: 183,
                reason: "Session Progress".into(),
                body: sdp.clone(),
                content_type: None,
                to_tag: None,
                p_early_media: Some("sendrecv".into()),
            }],
        );
        assert_eq!(result.effects.outbound.len(), 1);
        let eff = &result.effects.outbound[0];
        assert_eq!(eff.leg_id.as_deref(), Some("a"));
        match &eff.body {
            OutboundBody::Response(r) => {
                assert_eq!(r.status, 183);
                assert_eq!(r.body, sdp, "the MRF SDP is brokered onto A");
                assert!(r.to.tag.is_some(), "183 carries a B2BUA-minted early to-tag");
                assert_eq!(
                    get_header(&r.headers, "content-type"),
                    Some("application/sdp"),
                    "an SDP body defaults to application/sdp"
                );
                assert_eq!(get_header(&r.headers, "p-early-media"), Some("sendrecv"));
            }
            _ => panic!("expected an outbound response"),
        }
        // The minted tag is persisted on the a-dialog for reuse on later 1xx.
        assert!(
            result.call.a_leg.dialogs.first().is_some_and(|d| !d.sip.local_tag.is_empty()),
            "the a-facing early tag is persisted"
        );
    }

    // A non-1xx status (or a non-a target) is rejected — no UAS transaction to
    // answer (port of leg-kind-gate test 5/6).
    #[test]
    fn send_provisional_rejects_non_provisional_status() {
        let call = test_call();
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
        };
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let result = exec_on(
            &call,
            &event,
            "a",
            &config,
            &id_gen,
            &[RuleAction::SendProvisionalToLeg {
                leg_id: "a".into(),
                status: 200,
                reason: "OK".into(),
                body: vec![],
                content_type: None,
                to_tag: None,
                p_early_media: None,
            }],
        );
        assert!(result.effects.outbound.is_empty(), "a non-1xx provisional is rejected");
    }
}
