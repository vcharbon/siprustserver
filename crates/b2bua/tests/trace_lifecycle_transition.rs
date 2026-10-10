// Own binary (ADR-0030 X2): installs the process trace registry (`install_process_traces`).
//! A cursor move a lifecycle handler makes reaches the trace (ADR-0026).
//!
//! A service hearing a lifecycle event the engine delivers after a turn's
//! claim may move its own machine's cursor. That move is a state-machine
//! transition like a claim's, and a traced call records it as one: a
//! `rule.transition` naming the handler, its machine and the edge, beside the
//! `rule.lifecycle` delivery itself.
//!
//! The trace registry is process-wide, so this file holds exactly ONE test and
//! installs its own gate.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
use b2bua::rules::lifecycle_delivery::{close_turn, TurnStart};
use b2bua::rules::{
    execute_rules, ActionExecutor, Match, RuleAction, RuleCall, RuleContext, RuleDefinition,
    RuleHandleResult, SERVICE_LAYER,
};
use b2bua::trace::{install_process_traces, traces, CallTraces};
use b2bua_sdk::event::CallEvent;
use call::{Direction, LegState, MachineId, StateLabel};
use observe::{activation_bucket, RateDraw, SampleAdmission};
use sip_message::parser::custom::CustomParser;
use sip_message::{SipMessage, SipParser};
use sip_txn::IdGen;

const MACHINE: &str = "listening-machine";

static ACTIVE_S0: [StateLabel; 1] = [StateLabel::new("S0")];
static S0_TO_S1: [(StateLabel, StateLabel); 1] = [(StateLabel::new("S0"), StateLabel::new("S1"))];

const INVITE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-lifecycle\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=alicetag\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: lifecycle-transition@10.0.0.9\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.9:5060>\r\n\
Content-Length: 0\r\n\r\n";

/// Hears a leg end and moves its own cursor S0 → S1.
fn on_leg_ended(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::SetState {
        machine: MachineId::new(MACHINE),
        to: StateLabel::new("S1"),
    }]))
}

/// A machine-bound leg-end listener, active in S0, declaring S0 → S1.
fn listener() -> RuleDefinition {
    RuleDefinition {
        id: "test-leg-end-listener",
        layer: SERVICE_LAYER,
        overrides: &[],
        matcher: Match::leg_ended(),
        handle: on_leg_ended,
        machine: Some(MachineId::new(MACHINE)),
        active_states: &ACTIVE_S0,
        transitions: &S0_TO_S1,
        effects: &[],
        teardown: false,
    }
}

/// Claims the internal event and refuses the caller, which ends her leg.
fn ending_claimer() -> RuleDefinition {
    RuleDefinition::core(
        "test-ending-claimer",
        SERVICE_LAYER,
        &[],
        Match::internal_event().topic("end-a"),
        |_| {
            Some(RuleHandleResult::new(vec![RuleAction::RespondToALeg {
                status: 486,
                reason: "Busy Here".into(),
                header_updates: vec![],
                contacts: vec![],
            }]))
        },
    )
}

/// A sampled call with an open root span and the listening machine at S0.
fn traced_call() -> call::Call {
    let invite = match CustomParser::new().parse(INVITE).expect("fixture INVITE parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    };
    let src: SocketAddr = "10.0.0.9:5060".parse().expect("fixture address");
    let mut call = build_initial_call(&invite, src, &B2buaConfig::default(), &IdGen::seeded(1), 0);
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    call.sm_cursors.insert(MachineId::new(MACHINE), StateLabel::new("S0"));
    traces()
        .activate(&call.call_ref, b2bua::trace::registry::call_identity(&call), Some(1.0), 0)
        .expect("the gate samples every call");
    call.sampled = Some(true);
    call
}

#[test]
fn a_lifecycle_handlers_cursor_move_is_on_the_trace() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(5), activation_bucket(0)),
        false,
    )));
    let (_log_guard, log) = observe::test_buffer();

    let call = traced_call();
    let event = CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: "end-a".into(),
        outcome: "ok".into(),
        payload: serde_json::json!({}),
        body: vec![],
        incarnation: None,
    };
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = [listener(), ending_claimer()];
    let claimed =
        execute_rules(&rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core());
    let result = close_turn(&rules, &TurnStart::of(&call), claimed, &exec);
    assert_eq!(
        result.call.sm_cursors.get(&MachineId::new(MACHINE)).map(StateLabel::as_str),
        Some("S1"),
        "the listener moved its cursor",
    );

    assert!(
        log.matching("kind=rule.lifecycle")
            .iter()
            .any(|e| e.contains("test-leg-end-listener leg-ended")),
        "the call is traced: the delivery itself is on the trace",
    );
    let transitions = log.matching("kind=rule.transition");
    assert!(
        transitions.iter().any(|e| e.contains("test-leg-end-listener listening-machine: S0 -> S1")),
        "the listener's move is a traced transition: {:?}",
        transitions.iter().map(|e| e.line()).collect::<Vec<_>>(),
    );

    traces().close(&call.call_ref);
}
