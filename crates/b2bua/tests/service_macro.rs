//! ADR-0016 slice 3 — `define_service!` / `sm_rule!` macro + `init` hook.
//!
//! A test-only **stub service** (`S0 → S1`, one `init`, one state-gated rule)
//! exercised end-to-end with no real service: the macro generates the state enum
//! + machine + rules + init; `seed_services` seeds the cursor (S0) and the data
//! backing; the rule fires in S0, `SetState`s to S1, and declines in S1 — proving
//! macro + init + selection compose.

use std::net::SocketAddr;

use b2bua::config::B2buaConfig;
use b2bua::effects::HandlerResult;
use b2bua::event::CallEvent;
use b2bua::initial_invite::build_initial_call;
use b2bua::rules::{
    execute_rules, pick_ranked, seed_services, ActionExecutor, Match, RuleAction, RuleContext,
    RuleDefinition, RuleHandleResult, ServiceSeed,
};
use b2bua::{define_service, sm_rule};
use call::{Call, Direction, MachineId, StateLabel};
use sip_message::generators::{
    generate_out_of_dialog_request, ContactSpec, GenerateOutOfDialogRequestOpts, OutOfDialogMethod,
    SipTransport, ViaSpec,
};
use sip_message::{SipMessage, SipRequest};
use sip_txn::IdGen;

/// The test-only stub service.
mod stub {
    use super::*;

    define_service! {
        id: "stub",
        machine: STUB_MACHINE,
        states: StubState { S0, S1 },
        // Always applicable in the test; a real service decides applicability
        // here (returning `None` to stay dormant). Seeds cursor=S0 + a data
        // marker (using `callback_context` as the stub's "data backing").
        init: |_call: &Call| {
            Some(
                ServiceSeed::new(StubState::S0.label())
                    .with_data(|c| c.callback_context = Some("stub-seeded".into())),
            )
        },
        rules: [ advance() ],
    }

    /// In S0, on an INFO, advance to S1.
    fn advance() -> RuleDefinition {
        sm_rule! {
            id: "stub-advance",
            machine: STUB_MACHINE,
            active: [ StubState::S0 ],
            transitions: [ StubState::S0 => StubState::S1 ],
            matcher: Match::request().method("INFO"),
            handle: |_ctx: &RuleContext| {
                Some(RuleHandleResult::new(vec![RuleAction::SetState {
                    machine: STUB_MACHINE,
                    to: StubState::S1.label(),
                }]))
            },
        }
    }
}

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

fn test_call() -> Call {
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    build_initial_call(&invite(), src, &B2buaConfig::default(), 0)
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

fn ctx_for<'a>(call: &'a Call, event: &'a CallEvent, config: &'a B2buaConfig) -> RuleContext<'a> {
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
fn stub_service_init_seeds_then_rule_advances_then_declines() {
    let machine = MachineId::new("stub");
    let cursor = |c: &Call| c.sm_cursors.get(&machine).map(StateLabel::as_str).map(str::to_string);

    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let event = info_event();
    let services = vec![stub::service_def()];

    // ── init: seeds cursor=S0 + data, folded through seed_services ──
    let seeded = seed_services(
        HandlerResult::new(test_call()),
        &services,
        &exec,
        &event,
        "a",
        Direction::FromA,
    );
    assert_eq!(cursor(&seeded.call).as_deref(), Some("S0"), "init seeded cursor=S0");
    assert_eq!(
        seeded.call.callback_context.as_deref(),
        Some("stub-seeded"),
        "init installed the data backing"
    );

    // ── selection + SetState: the rule is a candidate in S0 and advances to S1 ──
    let rules = stub::rules();
    let advanced = {
        let ctx = ctx_for(&seeded.call, &event, &config);
        assert_eq!(
            pick_ranked(&rules, &ctx).first().map(|r| r.id),
            Some("stub-advance"),
            "candidate in S0"
        );
        execute_rules(&rules, &ctx, &exec, |c: &RuleContext| HandlerResult::new(c.call.clone()))
    };
    assert_eq!(cursor(&advanced.call).as_deref(), Some("S1"), "SetState moved cursor to S1");

    // ── declines in S1: the S0-gated rule is no longer a candidate ──
    {
        let ctx = ctx_for(&advanced.call, &event, &config);
        assert!(pick_ranked(&rules, &ctx).is_empty(), "rule declines in S1");
    }
}

/// A dormant service (`init` → `None`) costs a vanilla call nothing: no cursor,
/// no data, and `compose_rules` with no services equals the bare core list.
#[test]
fn dormant_service_leaves_call_untouched() {
    use b2bua::rules::{compose_rules, default_rules, ServiceDef};

    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor { config: &config, id_gen: &id_gen, now_ms: 0 };
    let event = info_event();

    fn dormant_init(_: &Call) -> Option<ServiceSeed> {
        None
    }
    let dormant = ServiceDef { id: "dormant", init: dormant_init, rules: Vec::new };
    let services = vec![dormant];

    let before = test_call();
    let after = seed_services(
        HandlerResult::new(before.clone()),
        &services,
        &exec,
        &event,
        "a",
        Direction::FromA,
    );
    assert_eq!(after.call, before, "dormant service touches nothing");

    // Empty composition is exactly the core list.
    let composed = compose_rules(&[], default_rules());
    assert_eq!(composed.len(), default_rules().len());
}
