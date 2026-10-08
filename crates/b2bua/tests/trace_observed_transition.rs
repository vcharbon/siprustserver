// Own binary (ADR-0030 X2): installs the process trace registry (`install_process_traces`).
//! A cursor move an observing rule makes reaches the trace (ADR-0026).
//!
//! A rule that observes an event without claiming it may move its own
//! machine's cursor ([`RuleHandleResult::observe`]). That move is a state-machine
//! transition like a claim's, and a traced call records it as one: a
//! `rule.transition` naming the observer, its machine and the edge.
//!
//! The trace registry is process-wide, so this file holds exactly ONE test and
//! installs its own gate.

use std::net::SocketAddr;
use std::sync::Arc;

use b2bua::config::B2buaConfig;
use b2bua::initial_invite::build_initial_call;
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

const MACHINE: &str = "observing-machine";

static ACTIVE_S0: [StateLabel; 1] = [StateLabel::new("S0")];
static S0_TO_S1: [(StateLabel, StateLabel); 1] = [(StateLabel::new("S0"), StateLabel::new("S1"))];

const INVITE: &[u8] = b"INVITE sip:bob@example.com SIP/2.0\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bK-observed\r\n\
Max-Forwards: 70\r\n\
From: <sip:alice@example.com>;tag=alicetag\r\n\
To: <sip:bob@example.com>\r\n\
Call-ID: observed-transition@10.0.0.9\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:alice@10.0.0.9:5060>\r\n\
Content-Length: 0\r\n\r\n";

/// Observes the event and moves its own cursor S0 → S1.
fn observe_moving(_ctx: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::observe(vec![RuleAction::SetState {
        machine: MachineId::new(MACHINE),
        to: StateLabel::new("S1"),
    }]))
}

/// A machine-bound observer of internal events, active in S0, declaring S0 → S1.
fn observer() -> RuleDefinition {
    RuleDefinition {
        id: "test-cursor-observer",
        layer: SERVICE_LAYER,
        overrides: &[],
        matcher: Match::internal_event().topic("observed"),
        handle: observe_moving,
        machine: Some(MachineId::new(MACHINE)),
        active_states: &ACTIVE_S0,
        transitions: &S0_TO_S1,
        effects: &[],
        teardown: false,
    }
}

/// A sampled call with an open root span and the observing machine at S0.
fn traced_call() -> call::Call {
    let invite = match CustomParser::new().parse(INVITE).expect("fixture INVITE parses") {
        SipMessage::Request(r) => r,
        SipMessage::Response(_) => panic!("expected a request"),
    };
    let src: SocketAddr = "10.0.0.9:5060".parse().expect("fixture address");
    let mut call = build_initial_call(&invite, src, &B2buaConfig::default(), &IdGen::seeded(1), 0);
    call.a_leg.state = LegState::Early;
    call.sm_cursors.insert(MachineId::new(MACHINE), StateLabel::new("S0"));
    traces()
        .activate(&call.call_ref, b2bua::trace::registry::call_identity(&call), Some(1.0), 0)
        .expect("the gate samples every call");
    call.sampled = Some(true);
    call
}

#[test]
fn an_observers_cursor_move_is_on_the_trace() {
    install_process_traces(Arc::new(CallTraces::new(
        SampleAdmission::new(true, 1.0, 200, RateDraw::seeded(5), activation_bucket(0)),
        false,
    )));
    let (_log_guard, log) = observe::test_buffer();

    let call = traced_call();
    let event = CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: "observed".into(),
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
    };
    let result = execute_rules(
        &[observer()],
        &call,
        &ctx,
        &exec,
        &b2bua::obligations::ObligationSet::core(),
    );
    assert_eq!(
        result.call.sm_cursors.get(&MachineId::new(MACHINE)).map(StateLabel::as_str),
        Some("S1"),
        "the observer moved its cursor",
    );

    assert!(
        !log.matching("kind=rule.observed").is_empty(),
        "the call is traced: the observation itself is on the trace",
    );
    let transitions = log.matching("kind=rule.transition");
    assert!(
        transitions.iter().any(|e| e.contains("test-cursor-observer observing-machine: S0 -> S1")),
        "the observer's move is a traced transition: {:?}",
        transitions.iter().map(|e| e.line()).collect::<Vec<_>>(),
    );

    traces().close(&call.call_ref);
}
