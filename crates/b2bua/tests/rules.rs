//! Rule-engine unit tests: matcher ranking + overrides, invariant enforcement,
//! and a representative default-rule's action output — pinned at the rule seam
//! without a full SUT.

use std::net::SocketAddr;

use b2bua::config::B2buaConfig;
use b2bua::effects::{BufferedObservabilityEffect, CriticalStateEffect, HandlerResult};
use b2bua::event::CallEvent;
use b2bua::initial_invite::build_initial_call;
use b2bua::rules::{
    default_rules, execute_rules, invariants, pick_ranked, ActionExecutor, Effect, Match,
    RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, SERVICE_LAYER,
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
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &B2buaConfig::default(),
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
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
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &B2buaConfig::default(),
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("relay-bye"));
}

#[test]
fn invariants_append_cleanup_on_terminated() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    call.a_leg.state = LegState::Terminated;
    let result = invariants::enforce(
        &b2bua::obligations::ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        true,
    );

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
    // ADR-0022: `before` entered the turn with the a-leg still Trying and the
    // turn answered nothing — the funnel appends the forgotten final (503) and
    // records it on the CDR.
    assert!(
        result.effects.outbound.iter().any(|e| {
            e.leg_id.as_deref() == Some("a")
                && matches!(&e.body, b2bua::effects::OutboundBody::Response(r) if r.status == 503)
        }),
        "unanswered a-leg gets the synthesized 503"
    );
    assert!(
        result.call.cdr_events.iter().any(|e| {
            e.reason.as_deref() == Some("unanswered_at_termination") && e.status_code == Some(503)
        }),
        "the synthesized final is on the CDR"
    );
}

#[test]
fn no_synthesized_final_when_the_turn_already_answered() {
    // The reject path: same terminated transition, but THIS turn carries a
    // final to the a-leg (as `reject_call` / `RespondToALeg` do) — the funnel
    // must not double-answer.
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    call.a_leg.state = LegState::Terminated;
    let a_invite = b2bua::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let mut result = HandlerResult::new(call);
    result.effects.outbound.push(b2bua::rules::relay::response_to_a_leg(
        &a_invite, 486, "Busy Here", Some("totag-x".into()), None, vec![], None, None, vec![],
    ));
    let result = invariants::enforce(
        &b2bua::obligations::ObligationSet::core(),
        &before,
        result,
        0,
        true,
    );
    let finals_to_a = result
        .effects
        .outbound
        .iter()
        .filter(|e| {
            e.leg_id.as_deref() == Some("a")
                && matches!(&e.body, b2bua::effects::OutboundBody::Response(r) if r.status >= 200)
        })
        .count();
    assert_eq!(finals_to_a, 1, "exactly the rule's own final — no synthesized duplicate");
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
/// A declared terminal edge: S0 deactivates the machine (ADR-0016 X9).
static SM_TRANSITIONS_TERMINAL: [(StateLabel, StateLabel); 1] =
    [(StateLabel::new("S0"), StateLabel::terminal())];

/// Deactivates the machine (removes the cursor) via `ClearState`.
fn handle_clears_state(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::ClearState {
        machine: MachineId::new(TEST_MACHINE),
    }]))
}

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

/// Emits a tracked `LegMessage` side effect (a final response to a leg).
fn handle_emits_leg_message(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::Respond {
        status: 200,
        reason: "OK".to_string(),
        body: vec![],
        content_type: None,
    }]))
}

/// One declared `LegMessage` effect (ADR-0016 X9 — the rule may respond to a leg).
static SM_EFFECTS_LEG_MESSAGE: [Effect; 1] =
    [Effect::Respond { status: 200, label: "200 → leg" }];

/// Like [`sm_rule`] but with a non-empty declared `effects` list.
fn sm_rule_with_effects(
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    effects: &'static [Effect],
) -> RuleDefinition {
    RuleDefinition {
        effects,
        ..sm_rule(handle)
    }
}

/// Like [`sm_rule`] but with a custom declared `transitions` list.
fn sm_rule_with_transitions(
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    transitions: &'static [(StateLabel, StateLabel)],
) -> RuleDefinition {
    RuleDefinition {
        transitions,
        ..sm_rule(handle)
    }
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
        effects: &[],
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
        call: RuleCall::new(call),
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
        assert!(pick_ranked(&rules, &call, &ctx).is_empty(), "dormant without a cursor");
    }

    // Cursor in `active_states` (S0) → candidate.
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    {
        let ctx = ctx_for(&call, &event, &config);
        assert_eq!(
            pick_ranked(&rules, &call, &ctx).first().map(|r| r.id),
            Some("test-sm-rule"),
            "candidate when cursor ∈ active_states"
        );
    }

    // Cursor outside `active_states` (S1) → skipped.
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S1"));
    {
        let ctx = ctx_for(&call, &event, &config);
        assert!(pick_ranked(&rules, &call, &ctx).is_empty(), "skipped when cursor ∉ active_states");
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
        execute_rules(&rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core())
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
        pick_ranked(&rules, &next, &ctx2).is_empty(),
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
    let _ = execute_rules(&rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core());
}

/// A tracked side effect the handler emits but the rule did not declare trips the
/// debug drift-check (ADR-0016 X9), the effect analogue of the transition check.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "undeclared")]
fn undeclared_effect_trips_debug_assert() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    // `sm_rule` declares `effects: &[]`, but the handler emits a `LegMessage`.
    let rules = vec![sm_rule(handle_emits_leg_message)];

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core());
}

/// Declaring the matching `LegMessage` effect satisfies the drift-check — the
/// same emit no longer trips it (the by-category `emitted ⊆ declared` contract).
#[test]
fn declared_effect_passes_the_drift_check() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let rules = vec![sm_rule_with_effects(handle_emits_leg_message, &SM_EFFECTS_LEG_MESSAGE)];

    let ctx = ctx_for(&call, &event, &config);
    // Must not panic: the emitted Respond is a declared LegMessage.
    let _ = execute_rules(&rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core());
}

/// `ClearState` (machine deactivation) removes the cursor, and with a declared
/// `S0 => terminal` edge the transition drift-check accepts it (ADR-0016 X9).
#[test]
fn declared_terminal_clear_state_deactivates_machine() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let rules = vec![sm_rule_with_transitions(handle_clears_state, &SM_TRANSITIONS_TERMINAL)];

    let ctx = ctx_for(&call, &event, &config);
    let r = execute_rules(&rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core());
    // The machine is deactivated: its cursor is gone.
    assert!(!r.call.sm_cursors.contains_key(&MachineId::new(TEST_MACHINE)));
}

/// A `ClearState` whose `S0 => terminal` edge is **not** declared trips the
/// transition drift-check (only `S0 => S1` is in `SM_TRANSITIONS`).
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "undeclared transition")]
fn undeclared_terminal_clear_state_trips_debug_assert() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let rules = vec![sm_rule(handle_clears_state)]; // transitions: S0 => S1 only

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core());
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
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);

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

// Regression for the cluster double-record-route reboot-loss: the front proxy's
// two Record-Route halves (cookie + `;outbound`) arrive on the wire COMBINED in a
// single Record-Route header (RFC 3261 §7.3.1). The §12.1.2 reversal must operate
// on individual route URIs, not header lines — otherwise reversing one combined
// value is a no-op and leaves the cookie on top, so the worker→callee keepalive
// carries the cookie first and the proxy bounces it back to a worker after a
// reboot (no `;outbound` rescue). The b-leg route set MUST end up `;outbound`
// first so direction is intrinsic to the proxy's own Record-Route.
#[test]
fn confirm_dialog_splits_combined_record_route_and_puts_outbound_first() {
    let mut call = test_call();
    call = call::helpers::add_b_leg(call, b_leg_pending());

    // Worker-outbound b-leg INVITE → the proxy inserts [cookie, outbound]; the 2xx
    // echoes them comma-COMBINED in one header. §12.1.2 reverse of the individual
    // URIs → [outbound, cookie].
    let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Record-Route: <sip:10.0.0.9:5060;e=0;kid=k0;sig=ABC;v=3;w_bak=w1;w_pri=w0;lr>, <sip:10.0.0.9:5060;outbound;lr>\r\n\
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
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);
    let rs = &result.call.b_legs[0].dialogs[0].sip.route_set;
    assert_eq!(rs.len(), 2, "combined header must be split into 2 individual routes: {rs:?}");
    assert!(
        rs[0].contains("outbound"),
        "the proxy's `;outbound` half MUST be on top of the worker's b-leg route set (got {:?})",
        rs[0]
    );
    assert!(rs[1].contains("w_pri="), "the cookie half is second (got {:?})", rs[1]);
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
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);

    assert!(
        result.call.b_legs[0].dialogs[0].sip.route_set.is_empty(),
        "no Record-Route on the 2xx → b-leg route set stays empty"
    );
}

// ── CANCEL follows the INVITE's route set + next hop (RFC 3261 §9.1) ─────────
//
// Via-LB topology: the b-leg egresses through the front proxy
// (`b2b_outbound_proxy`), so the b-leg INVITE carries a preloaded outbound-proxy
// Route and its wire destination is the proxy. When the B2BUA later cancels the
// pending INVITE (no-answer teardown / reroute-on-486), §9.1 requires the CANCEL
// to take the SAME path — same next hop (the proxy, NOT `leg.source`, the
// callee's advertised address) and the same Route set, with the transaction-
// correlation Via branch echoed verbatim. Regression for GAP-P6-2: previously
// `generate_cancel` dropped the Route and `cancel_to_leg` sent to `leg.source`,
// so the CANCEL bypassed the proxy and never reached the pending server txn.
#[test]
fn cancel_follows_invite_route_set_and_next_hop_through_the_outbound_proxy() {
    use b2bua::effects::OutboundBody;

    let mut config = B2buaConfig::default();
    config.b2b_outbound_proxy = Some(("proxy.example".to_string(), 5060));

    let call = test_call();
    let a_invite = b2bua::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let id_gen = IdGen::seeded(7);
    // The callee's own address — what `leg.source` becomes; the CANCEL must NOT
    // go here (it must go to the proxy).
    let callee_dest = ("10.0.0.2".to_string(), 5070u16);
    let (leg, invite_effect) = b2bua::rules::relay::build_b_leg(
        &call.call_ref,
        "b-1",
        false,
        &a_invite,
        callee_dest.clone(),
        None,
        None,
        None,
        None,
        &config,
        &id_gen,
        None,
        &[],
        None,
    );
    // Sanity: the topology is via-LB — the INVITE itself went to the proxy.
    assert_eq!(
        invite_effect.destination,
        ("proxy.example".to_string(), 5060),
        "b-leg INVITE egresses through the front proxy"
    );
    let invite_via = match &invite_effect.body {
        OutboundBody::Request(r) => r
            .headers
            .iter()
            .find(|h| h.name.eq_ignore_ascii_case("via"))
            .map(|h| h.value.clone())
            .expect("INVITE has a Via"),
        _ => panic!("INVITE is a request"),
    };

    let call = call::helpers::add_b_leg(call, leg);

    // CancelLeg is action-driven and does not read the event; any event works.
    let event = CallEvent::Timer {
        timer_type: TimerType::NoAnswer,
        call_ref: call.call_ref.clone(),
        leg_id: Some("b-1".to_string()),
    };
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
    };
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let result = exec.execute(&[RuleAction::CancelLeg { leg_id: "b-1".into() }], &call, &ctx);

    let cancel_effect = result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method == "CANCEL"))
        .expect("a CANCEL was emitted");

    // (b) The CANCEL arrives AT THE PROXY (the INVITE's next hop), not pod-direct
    // at the callee (`leg.source`).
    assert_eq!(
        cancel_effect.destination,
        ("proxy.example".to_string(), 5060),
        "CANCEL must follow the INVITE's next hop (the proxy), not leg.source {callee_dest:?}"
    );

    let cancel = match &cancel_effect.body {
        OutboundBody::Request(r) => r,
        _ => unreachable!(),
    };
    // (a) The CANCEL carries the INVITE's Route set (the preloaded proxy Route).
    let routes: Vec<String> = cancel
        .headers
        .iter()
        .filter(|h| h.name.eq_ignore_ascii_case("route"))
        .map(|h| h.value.clone())
        .collect();
    assert_eq!(
        routes,
        vec!["<sip:proxy.example:5060;lr>".to_string()],
        "CANCEL must echo the INVITE's preloaded outbound-proxy Route (RFC 3261 §9.1)"
    );
    // ... and the transaction-correlation Via branch is the INVITE's verbatim.
    let cancel_via = cancel
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("via"))
        .map(|h| h.value.clone())
        .expect("CANCEL has a Via");
    assert_eq!(cancel_via, invite_via, "CANCEL top Via (incl. branch) must equal the INVITE's");
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
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event,
            source_leg_id,
            direction: Direction::FromB,
            now_ms: 0,
            config,
        };
        exec.execute(actions, call, &ctx)
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
                headers: vec![],
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

    // A service re-originating an in-dialog request forwards arbitrary application
    // headers verbatim onto the request — the seam a deferred INFO_UUI RELAY uses
    // to carry a held `User-To-User` toward the peer at an async decision re-entry
    // (newkahneed/021). Body-owned headers (Content-Type/Content-Length) listed
    // here are dropped, never duplicated: `body`/`content_type` own those.
    #[test]
    fn send_request_to_leg_forwards_arbitrary_headers() {
        let mut call = test_call();
        call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Media));
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
        };
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let uui = "3030373946313233;encoding=hex"; // RFC 7433 User-To-User
        let body = b"SUP:orangeindata\x00\x01payload".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::SendRequestToLeg {
                leg_id: "b-1".into(),
                method: "INFO".into(),
                body: body.clone(),
                content_type: Some("application/orangeindata".into()),
                headers: vec![
                    ("User-To-User".into(), uui.into()),
                    ("X-Orange-Trace".into(), "abc-123".into()),
                    // A body-owned header MUST be ignored — it is owned by
                    // `content_type` and must never be emitted twice.
                    ("Content-Type".into(), "text/bogus".into()),
                ],
            }],
        );
        assert_eq!(result.effects.outbound.len(), 1);
        let eff = &result.effects.outbound[0];
        assert_eq!(eff.leg_id.as_deref(), Some("b-1"));
        match &eff.body {
            OutboundBody::Request(r) => {
                assert_eq!(r.method, "INFO");
                assert_eq!(r.body, body, "opaque body passes through unchanged");
                // Forwarded application headers survive verbatim.
                assert_eq!(get_header(&r.headers, "user-to-user"), Some(uui));
                assert_eq!(get_header(&r.headers, "x-orange-trace"), Some("abc-123"));
                // Content-Type is owned by `content_type`: exactly one, from the
                // body — NOT the bogus forwarded one (dedup guard).
                let cts: Vec<&str> = r
                    .headers
                    .iter()
                    .filter(|h| h.name.eq_ignore_ascii_case("content-type"))
                    .map(|h| h.value.as_str())
                    .collect();
                assert_eq!(
                    cts,
                    vec!["application/orangeindata"],
                    "the body's Content-Type wins; a forwarded Content-Type is dropped, not duplicated"
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

// ── AnswerALegNewDialog: a-side fork-confirm (RFC 3261 §12.1 / RFC 3264 §5.1) ──
//
// The MRF / RBT early-media callflow answers ONE caller INVITE in two stages
// with two *different* To-tags: 183 (SDP-MRF, tag A1) then 200 (SDP-B, tag A2 ≠
// A1). `AnswerALegNewDialog` mints/adopts A2, supersedes the early a-dialog A1,
// relays the callee 200/SDP under A2, and confirms the a-leg (GAP-M-A / newkahsip
// MGIT spine).
mod answer_a_leg_new_dialog {
    use super::*;
    use b2bua::effects::OutboundBody;
    use sip_message::message_helpers::get_header;

    /// A call whose a-leg already carries the MRF early-media dialog A1 (the tag
    /// pinned by the media-leg `ConfirmDialog` / a prior 183).
    fn call_with_early_a_dialog(a1: &str) -> call::Call {
        let mut call = test_call();
        call.a_leg.dialogs = vec![Dialog {
            sip: StackDialog {
                call_id: call.a_leg.call_id.clone(),
                local_tag: a1.into(),
                remote_tag: call.a_leg.from_tag.clone(),
                local_uri: "sip:bob@host".into(),
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
        call
    }

    /// The action ignores the triggering event (it operates on the call), so any
    /// event drives it.
    fn some_event() -> CallEvent {
        CallEvent::Sip {
            message: Box::new(SipMessage::Request(super::invite())),
            src: "127.0.0.1:5060".parse().unwrap(),
        }
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
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event,
            source_leg_id,
            direction: Direction::FromB,
            now_ms: 0,
            config,
        };
        exec.execute(actions, call, &ctx)
    }

    // The callee 200 answers the a-leg under a FRESH To-tag A2 ≠ the early A1:
    // the a-dialog local_tag is re-stamped to A2, the SDP-B rides the 200, the
    // answer SDP is cached for §13.3.1.4, and the a-leg is confirmed.
    #[test]
    fn answers_under_a_fresh_tag_superseding_the_early_dialog() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let sdp_b = b"v=0\r\no=callee 2 2 IN IP4 10.0.0.70\r\n".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: sdp_b.clone(),
                content_type: None,
                to_tag: None,
                header_updates: vec![],
            }],
        );
        assert_eq!(result.effects.outbound.len(), 1);
        let eff = &result.effects.outbound[0];
        assert_eq!(eff.leg_id.as_deref(), Some("a"));
        let a2 = match &eff.body {
            OutboundBody::Response(r) => {
                assert_eq!(r.status, 200);
                assert_eq!(r.body, sdp_b, "the callee SDP-B rides the 200");
                assert_eq!(
                    get_header(&r.headers, "content-type"),
                    Some("application/sdp"),
                    "an SDP body defaults to application/sdp"
                );
                let tag = r.to.tag.clone().expect("the 200 carries an a-facing To-tag");
                assert_ne!(tag, "A1early", "A2 ≠ the early-media tag A1 (RFC 3264 §5.1)");
                tag
            }
            _ => panic!("expected an outbound response"),
        };
        let d = result.call.a_leg.dialogs.first().expect("a-dialog present");
        assert_eq!(d.sip.local_tag, a2, "the a-dialog local_tag is re-stamped to A2");
        assert_eq!(
            d.ext.cached_sdp.as_deref(),
            Some(sdp_b.as_slice()),
            "the answer SDP is cached for a §13.3.1.4 un-ACKed-2xx retransmit"
        );
        assert_eq!(result.call.a_leg.state, LegState::Confirmed, "the a-leg is confirmed");
    }

    // An explicit `to_tag` is used verbatim; `header_updates` add non-structural
    // headers (same discipline as `RespondToALeg`).
    #[test]
    fn honors_an_explicit_tag_and_header_updates() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: vec![],
                content_type: None,
                to_tag: Some("A2explicit".into()),
                header_updates: vec![("X-Served-By".into(), Some("mrf".into()))],
            }],
        );
        let eff = &result.effects.outbound[0];
        match &eff.body {
            OutboundBody::Response(r) => {
                assert_eq!(r.to.tag.as_deref(), Some("A2explicit"), "the supplied A2 is used verbatim");
                assert_eq!(get_header(&r.headers, "x-served-by"), Some("mrf"));
            }
            _ => panic!("expected an outbound response"),
        }
        assert_eq!(
            result.call.a_leg.dialogs.first().map(|d| d.sip.local_tag.as_str()),
            Some("A2explicit"),
        );
    }

    // A non-2xx status establishes no dialog — the primitive is a no-op (the
    // abandoned early dialog / the ADR-0022 unanswered-a-leg funnel own failure).
    #[test]
    fn non_2xx_status_is_a_no_op() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 486,
                reason: "Busy Here".into(),
                body: vec![],
                content_type: None,
                to_tag: None,
                header_updates: vec![],
            }],
        );
        assert!(result.effects.outbound.is_empty(), "a non-2xx final establishes no a-dialog");
        assert_eq!(
            result.call.a_leg.dialogs.first().map(|d| d.sip.local_tag.as_str()),
            Some("A1early"),
            "the early dialog A1 is left untouched",
        );
    }
}

// ── AckLeg body + Content-Type (RFC 3261 §13.2.2.4 delayed-offer answer) ─────
//
// `RuleAction::AckLeg` carries an optional body so a rule can ACK a 2xx with an
// SDP answer, completing a delayed-offer exchange (RFC 3264 §4 — the answer
// rides the ACK). A body-bearing ACK defaults its Content-Type to
// `application/sdp`; an explicit type overrides it; an empty body stays a bare
// ACK (no body, no Content-Type — the pre-body behaviour, unchanged). The body
// is a binary-safe `Vec<u8>`.
mod ack_leg_body {
    use super::*;
    use b2bua::effects::OutboundBody;
    use sip_message::message_helpers::get_header;

    /// A confirmed b-leg (callee tag learned) whose first dialog `AckLeg` addresses.
    fn b_leg_confirmed() -> Leg {
        let mut leg = b_leg_pending();
        leg.state = LegState::Confirmed;
        leg.disposition = LegDisposition::Bridged;
        if let Some(d) = leg.dialogs.first_mut() {
            d.sip.remote_tag = "bobtag".into();
        }
        leg
    }

    /// Execute one `AckLeg { leg_id: "b-1", body, content_type }` against a call
    /// carrying a confirmed `b-1`, and return the emitted ACK. `AckLeg` reads only
    /// the call + the action fields (not the event), so any event serves.
    fn ack_request(body: Vec<u8>, content_type: Option<String>) -> SipRequest {
        let call = call::helpers::add_b_leg(test_call(), b_leg_confirmed());
        let config = B2buaConfig::default();
        let event = CallEvent::Timer {
            timer_type: TimerType::NoAnswer,
            call_ref: call.call_ref.clone(),
            leg_id: Some("b-1".to_string()),
        };
        let ctx = RuleContext {
            call: RuleCall::new(&call),
            call_ref: &call.call_ref,
            event: &event,
            source_leg_id: "b-1",
            direction: Direction::FromB,
            now_ms: 0,
            config: &config,
        };
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
        let result = exec.execute(
            &[RuleAction::AckLeg { leg_id: "b-1".into(), body, content_type }],
            &call,
            &ctx,
        );
        let effect = result
            .effects
            .outbound
            .iter()
            .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method == "ACK"))
            .expect("AckLeg emits an ACK request");
        match &effect.body {
            OutboundBody::Request(r) => r.clone(),
            _ => unreachable!(),
        }
    }

    // A non-empty body rides the ACK verbatim (binary-safe) and, absent an
    // explicit type, the ACK advertises `Content-Type: application/sdp`.
    #[test]
    fn ack_leg_carries_body_verbatim_and_defaults_content_type_to_sdp() {
        let mut body =
            b"v=0\r\no=- 0 0 IN IP4 10.0.0.9\r\ns=-\r\nc=IN IP4 10.0.0.9\r\nt=0 0\r\n\
              m=audio 40000 RTP/AVP 0\r\n"
                .to_vec();
        body.push(0xFF); // a non-UTF-8 byte must survive the Vec<u8> round-trip.

        let ack = ack_request(body.clone(), None);
        assert_eq!(
            ack.body, body,
            "the delayed-offer answer rides the ACK byte-for-byte (binary-safe)"
        );
        assert_eq!(
            get_header(&ack.headers, "content-type"),
            Some("application/sdp"),
            "a body-bearing ACK defaults Content-Type to application/sdp (§13.2.2.4)"
        );
    }

    // An explicit content_type overrides the application/sdp default.
    #[test]
    fn ack_leg_honours_explicit_content_type_override() {
        let body = b"<some>opaque</some>".to_vec();
        let ack = ack_request(body.clone(), Some("application/custom".to_string()));
        assert_eq!(ack.body, body);
        assert_eq!(
            get_header(&ack.headers, "content-type"),
            Some("application/custom"),
            "an explicit content_type is used verbatim (not coerced to application/sdp)"
        );
    }

    // Regression: an empty body sends a bare ACK with no Content-Type — the
    // behaviour before AckLeg grew a body, unchanged.
    #[test]
    fn ack_leg_empty_body_sends_bare_ack_with_no_content_type() {
        let ack = ack_request(Vec::new(), None);
        assert!(ack.body.is_empty(), "an empty AckLeg body sends a bodyless ACK");
        assert_eq!(
            get_header(&ack.headers, "content-type"),
            None,
            "a bare ACK carries no Content-Type (no regression vs the pre-body AckLeg)"
        );
    }
}

// ── TargetAdmission: the rule-driven `create-leg` gate (migration/26) ────────
//
// Port of `tests/b2bua/action-executor-create-leg-admission.test.ts` (source pin
// fffc4ac6). The `apply_route` decision-boundary gate is wired-tested e2e in
// `b2bua-harness/tests/target_admission_gate.rs`; this pins the OTHER admission
// site — the rule-path `ActionExecutor::CreateLeg` branch a service reaches in
// production via REFER (`transfer-http-allow`) or the announcement service when
// call-control hands back a bogus transfer/MRF host. Driving `ActionExecutor`
// directly (the Rust analogue of the TS `executeActions(...)`) reaches the reject
// branch that no existing harness create-leg exercises (every one routes to the
// IP literal `127.0.0.1`, which classifies `IpLiteral` and never rejects).
//
// A rejected create-leg must emit NO b-leg outbound, terminate the call, and
// write a `Reject` CDR carrying `admission_reject host=<host>` (the Rust analogue
// of the TS `admission_reject` span event — `HandlerEffects` has no span channel).
// IP literals and the `["*"]` wildcard admit regardless of the suffix list.
mod create_leg_admission {
    use super::*;
    use call::{CallModelState, CdrEventType};

    /// One `create-leg` action toward `host:port`, all overrides at their
    /// default (mirrors the TS `{ type: "create-leg", destination, fromInvite }`).
    fn create_leg(host: &str, port: u16) -> RuleAction {
        RuleAction::CreateLeg {
            destination: (host.into(), port),
            new_ruri: None,
            new_from: None,
            new_to: None,
            no_answer_timeout_sec: None,
            callback_context: None,
            body_override: None,
            header_updates: vec![],
            kind: None,
        }
    }

    /// Run a single `create-leg` from the a-leg under `config`'s suffix list and
    /// return the [`HandlerResult`]. A re-INVITE-shaped a-leg event is enough — the
    /// gate only reads `destination` + the config, exactly like the TS test's ctx.
    fn run_create_leg(config: &B2buaConfig, host: &str, port: u16) -> HandlerResult {
        let call = test_call();
        let reinvite = invite();
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(reinvite)),
            src: "127.0.0.1:5060".parse().unwrap(),
        };
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor { config, id_gen: &id_gen, now_ms: 1_700_000_000_000 };
        let ctx = RuleContext {
            call: RuleCall::new(&call),
            call_ref: &call.call_ref,
            event: &event,
            source_leg_id: "a",
            direction: Direction::FromA,
            now_ms: 1_700_000_000_000,
            config,
        };
        exec.execute(&[create_leg(host, port)], &call, &ctx)
    }

    // TS case 1: a rule routing to a non-IP non-suffixed host is rejected — no
    // b-leg outbound, the call is terminated, and a Reject CDR records the cause.
    #[test]
    fn create_leg_to_non_allow_listed_host_is_rejected() {
        let config = B2buaConfig::default(); // default suffix list [".svc.cluster.local"]
        let result = run_create_leg(&config, "kindlab", 5060);

        assert!(
            result.effects.outbound.is_empty(),
            "a rejected create-leg must emit no b-leg outbound (host never reaches the send path)"
        );
        assert!(
            result.call.b_legs.is_empty(),
            "no b-leg state is allocated on admission reject"
        );
        assert_eq!(
            result.call.state,
            CallModelState::Terminated,
            "the call is torn down (the Rust analogue of the TS terminate/remove-call effects)"
        );
        let reject = result
            .call
            .cdr_events
            .iter()
            .find(|e| e.event_type == CdrEventType::Reject)
            .expect("admission reject writes a Reject CDR event");
        assert_eq!(reject.status_code, Some(503));
        assert_eq!(
            reject.reason.as_deref(),
            Some("admission_reject host=kindlab"),
            "the Reject CDR carries the admission cause + host"
        );
    }

    // TS case 2: a create-leg to an IP literal is admitted regardless of the suffix
    // list — the b-leg INVITE is emitted and the call is NOT terminated.
    #[test]
    fn create_leg_to_ip_literal_is_admitted_regardless_of_suffix_list() {
        let config = B2buaConfig::default(); // 10.0.1.5 ∉ [".svc.cluster.local"] but is an IP
        let result = run_create_leg(&config, "10.0.1.5", 5060);

        assert!(
            !result.effects.outbound.is_empty(),
            "an IP-literal create-leg is admitted: the b-leg INVITE is emitted"
        );
        assert_eq!(result.call.b_legs.len(), 1, "the b-leg is created");
        assert_ne!(
            result.call.state,
            CallModelState::Terminated,
            "an admitted create-leg does not terminate the call"
        );
        assert!(
            !result.call.cdr_events.iter().any(|e| e.event_type == CdrEventType::Reject),
            "no Reject CDR on an admitted create-leg"
        );
    }

    // TS case 3: the `*` wildcard in the allow-list lets any host through — the
    // non-IP `kindlab` is now admitted.
    #[test]
    fn create_leg_to_any_host_is_admitted_under_wildcard_allow_list() {
        let config = B2buaConfig {
            worker_allowed_target_suffixes: vec!["*".into()],
            ..Default::default()
        };
        let result = run_create_leg(&config, "kindlab", 5060);

        assert!(
            !result.effects.outbound.is_empty(),
            "the wildcard admits even a non-IP host: the b-leg INVITE is emitted"
        );
        assert_eq!(result.call.b_legs.len(), 1, "the b-leg is created under the wildcard");
        assert_ne!(
            result.call.state,
            CallModelState::Terminated,
            "a wildcard-admitted create-leg does not terminate the call"
        );
    }
}

// ── default_sdp config → CreateLeg body_override (service fake-offer source) ──
//
// The `default_sdp` service parameter is a canned SDP a service sources to
// originate a deliberate *fake-offer* INVITE. The wiring is the existing
// `CreateLeg { body_override }` mechanism: the service passes
// `body_override: ctx.config.default_sdp.clone()` and `build_b_leg` stamps that
// body (+ `Content-Type: application/sdp`) onto the emitted b-leg INVITE.
// `default_sdp` is NEVER an automatic fallback — a normal reroute/failover
// `CreateLeg` (`body_override: None`) still relays the caller's own offer.
mod default_sdp_create_leg {
    use super::*;
    use b2bua::effects::OutboundBody;
    use sip_message::message_helpers::get_header;

    #[test]
    fn create_leg_sources_body_override_from_config_default_sdp() {
        let sdp = b"v=0\r\no=svc 42 42 IN IP4 10.0.0.9\r\ns=fake-offer\r\n\
                    c=IN IP4 10.0.0.9\r\nt=0 0\r\nm=audio 50000 RTP/AVP 8\r\n"
            .to_vec();
        // A service authoring a fake-offer INVITE parks the canned SDP on config.
        let config = B2buaConfig { default_sdp: Some(sdp.clone()), ..Default::default() };

        let call = test_call();
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(invite())),
            src: "127.0.0.1:5060".parse().unwrap(),
        };
        let ctx = RuleContext {
            call: RuleCall::new(&call),
            call_ref: &call.call_ref,
            event: &event,
            source_leg_id: "a",
            direction: Direction::FromA,
            now_ms: 0,
            config: &config,
        };
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };

        // The service sources the fake offer from the config parameter — the whole
        // opt-in wiring. (A normal reroute passes `body_override: None` here, which
        // relays the caller's own offer; `default_sdp` never substitutes itself.)
        let create = RuleAction::CreateLeg {
            destination: ("10.0.1.5".into(), 5070), // IP literal → admission passes
            new_ruri: None,
            new_from: None,
            new_to: None,
            no_answer_timeout_sec: None,
            callback_context: None,
            body_override: config.default_sdp.clone(),
            header_updates: vec![],
            kind: None,
        };
        let result = exec.execute(&[create], &call, &ctx);

        let invite_effect = result
            .effects
            .outbound
            .iter()
            .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method == "INVITE"))
            .expect("CreateLeg emits a b-leg INVITE");
        let inv = match &invite_effect.body {
            OutboundBody::Request(r) => r,
            _ => unreachable!(),
        };
        assert_eq!(
            inv.body, sdp,
            "the b-leg INVITE carries the config default_sdp sourced via body_override"
        );
        assert_eq!(
            get_header(&inv.headers, "content-type"),
            Some("application/sdp"),
            "the fake-offer INVITE advertises Content-Type: application/sdp"
        );
    }
}

// ── ADR-0020 X7: obligation-extraction equivalence gate ─────────────────────
//
// The limiter/CDR blocks of `invariants::enforce` were extracted verbatim into
// `obligations::{LimiterObligations, CdrObligation}`. This property test pins
// the refactor: over arbitrary call snapshots (states × limiter entries incl.
// fail-open × pre-emitted effects), the new `enforce(&ObligationSet::core(), …)`
// must produce an effect set and call identical to the pre-extraction body
// (kept VERBATIM below as the oracle).
mod enforce_equivalence {
    use super::*;
    use b2bua::effects::{
        BufferedObservabilityEffect, CriticalStateEffect, HandlerEffects, SoftBoundedEffect,
    };
    use b2bua::obligations::ObligationSet;
    use call::{CallLimiterState, TimerEntry};
    use proptest::prelude::*;
    use std::collections::HashSet;

    /// The pre-extraction `enforce` body (ADR-0010 X5 shape), verbatim.
    fn old_enforce(before: &call::Call, mut result: HandlerResult) -> HandlerResult {
        let became_terminated = before.state != CallModelState::Terminated
            && result.call.state == CallModelState::Terminated;
        if !became_terminated {
            return result;
        }
        let crit = &mut result.effects.critical;
        if !crit.iter().any(|e| matches!(e, CriticalStateEffect::CancelAllTimers)) {
            crit.insert(0, CriticalStateEffect::CancelAllTimers);
        }
        result.call.timers.clear();

        if !result
            .effects
            .buffered
            .iter()
            .any(|e| matches!(e, BufferedObservabilityEffect::WriteCdr))
        {
            result.effects.buffered.push(BufferedObservabilityEffect::WriteCdr);
        }

        let already: HashSet<(String, i64)> = result
            .effects
            .soft
            .iter()
            .map(|SoftBoundedEffect::DecrementLimiter { limiter_id, window }| {
                (limiter_id.clone(), *window)
            })
            .collect();
        for entry in &result.call.limiter_entries {
            if entry.increment_succeeded == Some(false) {
                continue;
            }
            let key = (entry.limiter_id.clone(), entry.origin_window);
            if already.contains(&key) {
                continue;
            }
            result.effects.soft.push(SoftBoundedEffect::DecrementLimiter {
                limiter_id: entry.limiter_id.clone(),
                window: entry.origin_window,
            });
        }

        result
            .effects
            .critical
            .retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
        result.effects.critical.push(CriticalStateEffect::RemoveCall);
        result
    }

    fn arb_state() -> impl Strategy<Value = CallModelState> {
        prop_oneof![
            Just(CallModelState::Active),
            Just(CallModelState::Terminating),
            Just(CallModelState::Terminated),
        ]
    }

    fn arb_limiter_entry() -> impl Strategy<Value = CallLimiterState> {
        (
            prop_oneof![Just("l1"), Just("l2")],
            0..3i64,
            prop_oneof![Just(None), Just(Some(true)), Just(Some(false))],
        )
            .prop_map(|(id, w, inc)| CallLimiterState {
                limiter_id: id.to_string(),
                limit: 10,
                origin_window: w * 100,
                increment_succeeded: inc,
            })
    }

    /// A pre-emitted rule decrement (possibly overlapping a recorded hold —
    /// the dedupe case — or naming a hold that does not exist).
    fn arb_pre_decrement() -> impl Strategy<Value = SoftBoundedEffect> {
        (prop_oneof![Just("l1"), Just("l2"), Just("lX")], 0..3i64).prop_map(|(id, w)| {
            SoftBoundedEffect::DecrementLimiter { limiter_id: id.to_string(), window: w * 100 }
        })
    }

    proptest! {
        #[test]
        fn extracted_enforce_is_equivalent_to_the_old_body(
            before_state in arb_state(),
            after_state in arb_state(),
            entries in proptest::collection::vec(arb_limiter_entry(), 0..4),
            pre_decrements in proptest::collection::vec(arb_pre_decrement(), 0..3),
            pre_write_cdr in proptest::bool::ANY,
            pre_cancel_all in proptest::bool::ANY,
            pre_remove_call in proptest::bool::ANY,
            timer_count in 0..3usize,
        ) {
            let mut before = test_call();
            before.state = before_state;

            let mut after = test_call();
            after.state = after_state;
            after.limiter_entries = entries;
            after.timers = (0..timer_count)
                .map(|i| TimerEntry {
                    id: format!("Keepalive:{i}"),
                    timer_type: TimerType::Keepalive,
                    fire_at: 1_000 + i as i64,
                    leg_id: None,
                })
                .collect();

            let mut effects = HandlerEffects::new();
            if pre_cancel_all {
                effects.critical.push(CriticalStateEffect::CancelAllTimers);
            }
            if pre_remove_call {
                effects.critical.push(CriticalStateEffect::RemoveCall);
            }
            effects.critical.push(CriticalStateEffect::CancelTimer { id: "NoAnswer:b-1".into() });
            effects.soft.extend(pre_decrements);
            if pre_write_cdr {
                effects.buffered.push(BufferedObservabilityEffect::WriteCdr);
            }

            let result = HandlerResult { call: after, effects };

            let old = old_enforce(&before, result.clone());
            // `answer_unanswered_a_leg = false`: this property pins the verbatim
            // limiter/CDR extraction against the pre-ObligationSet `old_enforce`;
            // the ADR-0022 unanswered-a-leg final is additive and covered by its
            // own tests above.
            let new = invariants::enforce(&ObligationSet::core(), &before, result, 0, false);

            prop_assert_eq!(&old.call, &new.call, "call (incl. cleared timers) must match");
            prop_assert_eq!(
                format!("{:?}", (&old.effects.critical, &old.effects.soft, &old.effects.buffered)),
                format!("{:?}", (&new.effects.critical, &new.effects.soft, &new.effects.buffered)),
                "effect lanes must be identical (order included)"
            );
        }
    }
}

// ── Service-owned timers (TimerType::Service — newkahneed 007) ───────────────
//
// End-to-end fire/cancel/wildcard behaviour lives in
// `b2bua-harness/tests/service_timers.rs`; here we pin the *identity* seams at
// the executor level: distinct keys are distinct ledger entries, a same-key
// re-schedule supersedes (replace, not append), the recipe-minted cancel
// removes exactly its own entry, and matcher scoping keeps core and foreign
// services out of a service's fires.
mod service_timers {
    use super::*;

    const SVC: MachineId = MachineId::new("svc-a");

    fn schedule(t: TimerType, secs: i64) -> RuleAction {
        RuleAction::ScheduleTimer { timer_type: t, delay_sec: secs, leg_id: None }
    }

    #[test]
    fn distinct_keys_coexist_and_same_key_reschedule_supersedes() {
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 1_000 };
        let call = test_call();
        let event = info_like_event(&call);
        let ctx = ctx_at(&call, &event, &config);

        let fast = TimerType::service(SVC, "fast");
        let slow = TimerType::service(SVC, "slow");
        let result = exec.execute(
            &[schedule(fast.clone(), 3), schedule(slow.clone(), 6)],
            &call,
            &ctx,
        );
        let ids: Vec<&str> = result.call.timers.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["Service:svc-a:fast", "Service:svc-a:slow"],
            "two keys → two distinct ledger entries (distinct persisted ids)",
        );

        // Re-arm the SAME key: replaced in place, not appended.
        let ctx2 = ctx_at(&result.call, &event, &config);
        let rearmed = exec.execute(&[schedule(fast.clone(), 10)], &result.call, &ctx2);
        assert_eq!(rearmed.call.timers.len(), 2, "same-key re-arm supersedes");
        let entry = rearmed
            .call
            .timers
            .iter()
            .find(|t| t.id == "Service:svc-a:fast")
            .expect("fast entry present");
        assert_eq!(entry.fire_at, 1_000 + 10_000, "the re-arm's deadline won");
        assert_eq!(entry.timer_type, fast);

        // Recipe-minted cancel removes exactly its own entry.
        let ctx3 = ctx_at(&rearmed.call, &event, &config);
        let cancelled =
            exec.execute(&[RuleAction::cancel_timer(&fast, None)], &rearmed.call, &ctx3);
        let ids: Vec<&str> = cancelled.call.timers.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, vec!["Service:svc-a:slow"], "cancel hits only its own key");
    }

    /// Matcher scoping: a core timer rule never sees a service fire; a
    /// service's exact-key / wildcard matchers never see core fires or another
    /// service's fires.
    #[test]
    fn service_fires_are_invisible_to_core_and_foreign_services() {
        let config = B2buaConfig::default();
        let call = test_call();

        let mine = TimerType::service(SVC, "fast");
        let service_fire = CallEvent::Timer {
            timer_type: mine.clone(),
            call_ref: call.call_ref.clone(),
            leg_id: None,
        };
        let core_fire = CallEvent::Timer {
            timer_type: TimerType::GlobalDuration,
            call_ref: call.call_ref.clone(),
            leg_id: None,
        };
        let foreign_fire = CallEvent::Timer {
            timer_type: TimerType::service(MachineId::new("svc-b"), "fast"),
            call_ref: call.call_ref.clone(),
            leg_id: None,
        };

        // No CORE rule matches a service fire (core rules pin unit variants).
        let ctx = ctx_at(&call, &service_fire, &config);
        assert!(
            pick_ranked(&default_rules(), &call, &ctx).is_empty(),
            "core stays ignorant of service timers",
        );

        let exact = Match::timer().timer_type(mine.clone());
        let wildcard = Match::timer().service_timers(SVC);
        for (name, m) in [("exact", &exact), ("wildcard", &wildcard)] {
            let ctx = ctx_at(&call, &service_fire, &config);
            assert!(m.accepts_columns(&ctx), "{name} matcher accepts its own fire");
            let ctx = ctx_at(&call, &core_fire, &config);
            assert!(!m.accepts_columns(&ctx), "{name} matcher rejects a core fire");
            let ctx = ctx_at(&call, &foreign_fire, &config);
            assert!(!m.accepts_columns(&ctx), "{name} matcher rejects another service's fire");
        }
        // Exact-key is exact: same service, different key → no match.
        let other_key_fire = CallEvent::Timer {
            timer_type: TimerType::service(SVC, "slow"),
            call_ref: call.call_ref.clone(),
            leg_id: None,
        };
        let ctx = ctx_at(&call, &other_key_fire, &config);
        assert!(!exact.accepts_columns(&ctx), "exact-key matcher rejects a sibling key");
        assert!(wildcard.accepts_columns(&ctx), "per-service wildcard accepts a sibling key");
    }

    fn info_like_event(call: &call::Call) -> CallEvent {
        CallEvent::Timer {
            timer_type: TimerType::service(SVC, "fast"),
            call_ref: call.call_ref.clone(),
            leg_id: None,
        }
    }

    fn ctx_at<'a>(
        call: &'a call::Call,
        event: &'a CallEvent,
        config: &'a B2buaConfig,
    ) -> RuleContext<'a> {
        RuleContext {
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event,
            source_leg_id: "a",
            direction: Direction::FromA,
            now_ms: 1_000,
            config,
        }
    }
}
