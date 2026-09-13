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
    RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, TimerDelay, SERVICE_LAYER,
};
use call::{
    B2buaDialogExt, CallModelState, Dialog, Direction, Leg, LegDisposition, LegKind, LegState,
    MachineId, RemoteInfo, StackDialog, StateLabel, TimerType,
};
use sip_message::generators::{
    generate_out_of_dialog_request, CapabilitySet, GenerateOutOfDialogRequestOpts,
    OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::SipStr;
use sip_message::{HeaderName, Method, SipMessage, SipParser, SipRequest};
use sip_txn::IdGen;

/// The URI a fixture names as text.
fn uri_of(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

fn invite() -> SipRequest {
    let opts = GenerateOutOfDialogRequestOpts {
        request_uri: Some(uri_of("sip:bob@127.0.0.1:5070")),
        call_id: "c1@alice".into(),
        from: Some(
            header::From::from_uri(uri_of("sip:alice@host")).with_tag(SipStr::from_static("atag")),
        ),
        to: Some(header::To::from_uri(uri_of("sip:bob@host"))),
        cseq: 1,
        via: Some(Via::udp("127.0.0.1", 5060).with_branch(SipStr::from_static("z9hG4bKalice"))),
        contact: Some(header::Contact::from_uri(
            Uri::sip_user("alice", "127.0.0.1").with_port(5060),
        )),
        max_forwards: Some(70),
        body: b"v=0\r\n".to_vec(),
        content_type: None,
        extra_headers: vec![],
    };
    generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts)
}

/// The INVITE re-issued as an in-dialog request of `method`: both dialog tags
/// present and the CSeq restated, which is what makes it a different
/// transaction rather than an edited INVITE.
fn in_dialog_request(method: sip_message::Method) -> sip_message::SipRequest {
    let inv = invite();
    let cseq = sip_message::header::CSeq::new(inv.cseq().seq(), method.clone());
    inv.thaw()
        .with_method(method)
        .set(inv.to().clone().with_tag("btag"))
        .set(cseq)
        .freeze()
        .expect("an in-dialog request of the same dialog is complete")
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
        discharged: None,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("max-duration"));
}

/// The `no-answer` rule's output for a fire naming `fired_leg_id` on `call`.
fn no_answer_result(call: &call::Call, fired_leg_id: &str) -> Vec<RuleAction> {
    let event = CallEvent::Timer {
        timer_type: TimerType::NoAnswer,
        call_ref: call.call_ref.clone(),
        leg_id: Some(fired_leg_id.into()),
    };
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: fired_leg_id,
        direction: Direction::FromB,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let no_answer = ranked.iter().find(|r| r.id == "no-answer").expect("no-answer is a candidate");
    (no_answer.handle)(&ctx).expect("no-answer handles its own timer").actions
}

#[test]
fn no_answer_fire_on_a_confirmed_call_absorbs_to_cancel_only() {
    // A reclaim-restored stale `NoAnswer` ledger entry firing on its own
    // answered leg must be absorbed: the only action is the scrub of the
    // spent entry.
    let mut call = test_call();
    call.a_leg.state = LegState::Confirmed;
    let mut b = b_leg_pending();
    b.state = LegState::Confirmed;
    call = call::helpers::add_b_leg(call, b);
    let actions = no_answer_result(&call, "b-1");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::CancelTimer { id }] if id == "NoAnswer:b-1"
        ),
        "absorb: exactly the canonical per-leg scrub, got {actions:?}",
    );
}

#[test]
fn no_answer_fire_naming_an_absent_leg_absorbs_to_cancel_only() {
    // A restored stale entry can name a leg displaced/destroyed before the
    // crash: firing DestroyLeg + /calls/failure against a dead leg id on a
    // possibly-answered call is the same bug class — absorb and scrub.
    let mut call = test_call();
    call.a_leg.state = LegState::Confirmed;
    let mut b = b_leg_pending();
    b.state = LegState::Confirmed;
    call = call::helpers::add_b_leg(call, b);
    let actions = no_answer_result(&call, "b-ghost");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::CancelTimer { id }] if id == "NoAnswer:b-ghost"
        ),
        "absorb: exactly the canonical per-leg scrub, got {actions:?}",
    );
}

#[test]
fn no_answer_fire_on_own_pending_leg_fires_despite_other_leg_confirmed() {
    // The guard is per-leg, not call-level: another leg being Confirmed does
    // not spend a fire whose OWN leg is still awaiting an answer — the normal
    // body (CDR timeout + DestroyLeg on the fired leg) runs.
    let mut call = test_call();
    let mut confirmed = b_leg_pending();
    confirmed.state = LegState::Confirmed;
    call = call::helpers::add_b_leg(call, confirmed);
    let mut pending = b_leg_pending();
    pending.leg_id = "b-2".into();
    pending.state = LegState::Trying;
    call = call::helpers::add_b_leg(call, pending);
    let actions = no_answer_result(&call, "b-2");
    match actions.as_slice() {
        [RuleAction::AddCdrEvent { leg_id, reason, .. }, RuleAction::DestroyLeg { leg_id: destroyed }, tail]
            if leg_id == "b-2"
                && reason.as_deref() == Some("no_answer_timeout")
                && destroyed == "b-2"
                && matches!(
                    tail,
                    RuleAction::BeginTermination { .. } | RuleAction::FailureAsyncHttp { .. }
                ) => {}
        other => panic!("genuine fire runs the normal no-answer body, got {other:?}"),
    }
}

#[test]
fn no_answer_fire_on_a_cancelling_leg_absorbs_to_cancel_only() {
    // A caller CANCEL leaves the b-leg at state=Trying, disposition=Cancelling
    //: state alone reads "still awaiting an answer", but the
    // leg is already going away — the fire must not consult /calls/failure nor
    // author a second final on the a-leg's completed transaction.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    let mut b = b_leg_pending();
    b.disposition = LegDisposition::Cancelling;
    call = call::helpers::add_b_leg(call, b);
    let actions = no_answer_result(&call, "b-1");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::CancelTimer { id }] if id == "NoAnswer:b-1"
        ),
        "absorb: exactly the canonical per-leg scrub, got {actions:?}",
    );
}

#[test]
fn no_answer_fire_on_a_terminating_call_absorbs_to_cancel_only() {
    // Call-scoped discriminator: once the call is Terminating, no leg awaits an
    // answer — a fire (e.g. a reclaim-restored stale entry) is spent even when
    // the named leg's own disposition never recorded the CANCEL.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    call.state = CallModelState::Terminating;
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let actions = no_answer_result(&call, "b-1");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::CancelTimer { id }] if id == "NoAnswer:b-1"
        ),
        "absorb: exactly the canonical per-leg scrub, got {actions:?}",
    );
}

/// The `handle-timeout` rule's output for a b-leg INVITE transaction timeout
/// naming `leg_id` on `call`.
fn handle_timeout_result(call: &call::Call, leg_id: &str) -> Vec<RuleAction> {
    let event = CallEvent::Timeout {
        branch: "z9hG4bKb1".into(),
        call_ref: Some(call.call_ref.clone()),
        leg_id: Some(leg_id.into()),
        method: Some("INVITE".into()),
        destination: None,
        timeout_kind: sip_txn::TimeoutKind::Transaction,
    };
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: leg_id,
        direction: Direction::FromB,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let handle_timeout =
        ranked.iter().find(|r| r.id == "handle-timeout").expect("handle-timeout is a candidate");
    (handle_timeout.handle)(&ctx).expect("handle-timeout handles the timeout").actions
}

#[test]
fn invite_timeout_on_a_cancelling_leg_resolves_locally_without_consult() {
    // A pending b-leg whose CANCEL is in flight owes nothing to its dead
    // INVITE transaction: the timeout resolves the leg locally — no
    // /calls/failure consult, no BeginTermination re-arm of the safety timer.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    call.state = CallModelState::Terminating;
    let mut b = b_leg_pending();
    b.disposition = LegDisposition::Cancelling;
    call = call::helpers::add_b_leg(call, b);
    let actions = handle_timeout_result(&call, "b-1");
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::TerminateLeg { leg_id, bye_disposition: Some(call::ByeDisposition::Cancelled) }]
                if leg_id == "b-1"
        ),
        "going-away leg resolves locally on its timeout, got {actions:?}",
    );
}

#[test]
fn invite_timeout_on_a_live_pending_leg_still_consults() {
    // The going-away guard is narrow: a live pending b-leg's transaction
    // timeout keeps the failover consult.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let actions = handle_timeout_result(&call, "b-1");
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::FailureAsyncHttp { .. })),
        "live pending b-leg timeout consults /calls/failure, got {actions:?}",
    );
}

#[test]
fn setup_timeout_fire_on_a_terminating_call_absorbs_to_cancel_only() {
    // A terminating call authors no new final on the a-leg's completed
    // transaction: the fire is spent — absorb and scrub, no 408.
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    let event = CallEvent::Timer {
        timer_type: TimerType::SetupTimeout,
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
        discharged: None,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    let setup_timeout =
        ranked.iter().find(|r| r.id == "setup-timeout").expect("setup-timeout is a candidate");
    let actions = (setup_timeout.handle)(&ctx).expect("setup-timeout handles its timer").actions;
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::CancelTimer { id }] if id == "SetupTimeout"
        ),
        "absorb: exactly the spent-entry scrub, got {actions:?}",
    );
}

/// The `max-duration` rule's output for a `GlobalDuration` fire on `call`.
fn max_duration_result(call: &call::Call) -> Vec<RuleAction> {
    let event = CallEvent::Timer {
        timer_type: TimerType::GlobalDuration,
        call_ref: call.call_ref.clone(),
        leg_id: None,
    };
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let max_duration =
        ranked.iter().find(|r| r.id == "max-duration").expect("max-duration is a candidate");
    (max_duration.handle)(&ctx).expect("max-duration handles its timer").actions
}

#[test]
fn max_duration_fire_on_a_terminating_call_absorbs_to_cancel_only() {
    // The cap crossing the terminating window (caller BYE'd just before it, or
    // a reclaim restored a stale entry): even the consult-eligible shape —
    // answered, subscribed, callback_context — is spent on a going-away call.
    // No `call_release` consult (a `reroute` outcome would dial a fresh b-leg),
    // no BeginTermination re-arm — absorb and scrub.
    let mut call = test_call();
    call.a_leg.state = LegState::Confirmed;
    call.state = CallModelState::Terminating;
    call.callback_context = Some("cb".into());
    call.subscriptions = vec![call::ReleaseEventKind::MaxCallDuration];
    let actions = max_duration_result(&call);
    assert!(
        matches!(
            actions.as_slice(),
            [RuleAction::CancelTimer { id }] if id == "GlobalDuration"
        ),
        "absorb: exactly the spent-entry scrub, got {actions:?}",
    );
}

#[test]
fn max_duration_fire_on_a_live_subscribed_call_still_consults() {
    // The going-away guard is narrow: an Active answered subscribed call's cap
    // keeps the `call_release` consult.
    let mut call = test_call();
    call.a_leg.state = LegState::Confirmed;
    call.callback_context = Some("cb".into());
    call.subscriptions = vec![call::ReleaseEventKind::MaxCallDuration];
    let actions = max_duration_result(&call);
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::ReleaseAsyncHttp { .. })),
        "live subscribed cap consults call_release, got {actions:?}",
    );
}

/// An internal event of `topic`/`outcome` carrying `payload` on `call`.
fn fold_event(
    call: &call::Call,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: topic.into(),
        outcome: outcome.into(),
        payload,
        body: Vec::new(),
    }
}

/// The ids the executor admits for the fold on `call`.
fn fold_candidates(
    call: &call::Call,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
) -> Vec<&'static str> {
    let event = fold_event(call, topic, outcome, payload);
    let config = B2buaConfig::default();
    let ctx = ctx_for(call, &event, &config);
    pick_ranked(&default_rules(), call, &ctx).iter().map(|r| r.id).collect()
}

/// The named decision-fold rule's own output for the fold on `call` — the
/// handler reached directly, whatever selection would admit.
fn fold_result(
    call: &call::Call,
    rule_id: &str,
    topic: &str,
    outcome: &str,
    payload: serde_json::Value,
) -> Vec<RuleAction> {
    let event = fold_event(call, topic, outcome, payload);
    let config = B2buaConfig::default();
    let ctx = ctx_for(call, &event, &config);
    let rules = default_rules();
    let rule = rules.iter().find(|r| r.id == rule_id).unwrap_or_else(|| panic!("{rule_id} exists"));
    (rule.handle)(&ctx).unwrap_or_else(|| panic!("{rule_id} handles its fold")).actions
}

/// A route-shaped fold payload (what `callouts::route_result_payload` emits).
fn route_fold_payload() -> serde_json::Value {
    serde_json::json!({
        "destination": { "host": "127.0.0.1", "port": 5070 },
        "failed_leg_id": "b-1",
    })
}

#[test]
fn decision_folds_on_a_terminating_call_are_dropped_whole() {
    // A `/calls` decision result landing on a call already going away drives
    // no forward progress, whatever it decides: no failover/reroute leg toward
    // a caller-less callee, no second final on the a-leg's completed
    // transaction (RFC 3261 §17.2.1), no re-termination. Two layers pin it:
    // the executor's going-away gate keeps every fold rule from being a
    // candidate at all, and each handler still drops the fold whole when
    // reached directly (defense in depth).
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    call.callback_context = Some("cb".into());
    call = call::helpers::add_b_leg(call, b_leg_pending());
    for (rule_id, topic, outcome, payload) in [
        ("failover-create-leg", "call-failure-result", "failover", route_fold_payload()),
        (
            "failover-reject",
            "call-failure-result",
            "reject",
            serde_json::json!({ "code": 484, "reason": "Address Incomplete" }),
        ),
        (
            "failover-redirect",
            "call-failure-result",
            "redirect",
            serde_json::json!({
                "code": 302,
                "contacts": [ { "uri": "sip:carol@10.0.0.3", "q": null } ],
            }),
        ),
        (
            "failover-terminate",
            "call-failure-result",
            "terminate",
            serde_json::json!({ "status": 486, "reason": "Busy Here" }),
        ),
        ("release-result-release", "call-release-result", "release", serde_json::json!({})),
        ("release-result-reroute", "call-release-result", "reroute", route_fold_payload()),
    ] {
        let candidates = fold_candidates(&call, topic, outcome, payload.clone());
        assert!(
            candidates.is_empty(),
            "the gate admits no fold rule on a terminating call, got {candidates:?}",
        );
        let actions = fold_result(&call, rule_id, topic, outcome, payload);
        assert!(
            actions.is_empty(),
            "{rule_id} on a terminating call is dropped whole, got {actions:?}",
        );
    }
}

#[test]
fn decision_folds_on_a_live_call_still_apply() {
    // The going-away guard is narrow: the same folds on an Active call keep
    // their normal bodies.
    let mut call = test_call();
    call.callback_context = Some("cb".into());
    call = call::helpers::add_b_leg(call, b_leg_pending());

    let actions = fold_result(
        &call,
        "failover-create-leg",
        "call-failure-result",
        "failover",
        route_fold_payload(),
    );
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::CreateLeg { .. })),
        "live failover fold creates the leg, got {actions:?}",
    );

    let actions = fold_result(
        &call,
        "failover-reject",
        "call-failure-result",
        "reject",
        serde_json::json!({ "code": 484, "reason": "Address Incomplete" }),
    );
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::RespondToALeg { status: 484, .. })),
        "live reject fold answers the caller, got {actions:?}",
    );

    // The reroute fold's live shape (answered + subscribed) is pinned by the
    // release-reroute suite; here the release outcome suffices as the control.
    let actions = fold_result(
        &call,
        "release-result-release",
        "call-release-result",
        "release",
        serde_json::json!({}),
    );
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
        "live release fold tears down, got {actions:?}",
    );
}

#[test]
fn begin_termination_scrubs_per_leg_no_answer_entries() {
    // Entering `terminating` cancels every per-leg NoAnswer ledger entry —
    // including the entry of a leg the teardown loop skips as already
    // `Cancelling` — so the fire is stopped at the source and a reclaim cannot
    // restore it into the terminating window.
    let mut call = test_call();
    let mut b = b_leg_pending();
    b.disposition = LegDisposition::Cancelling;
    call = call::helpers::add_b_leg(call, b);
    let no_answer_id = TimerType::NoAnswer.timer_id(Some("b-1"));
    call.timers.push(call::TimerEntry {
        id: no_answer_id.clone(),
        timer_type: TimerType::NoAnswer,
        fire_at: 5_000,
        leg_id: Some("b-1".into()),
    });
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
    };
    let config = B2buaConfig::default();
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
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let result = exec.execute(
        &[RuleAction::BeginTermination { reason: Some("CANCEL".into()) }],
        &call,
        &ctx,
    );
    assert!(
        !result.call.timers.iter().any(|t| t.timer_type == TimerType::NoAnswer),
        "no NoAnswer entry survives BeginTermination: {:?}",
        result.call.timers,
    );
    assert!(
        result
            .effects
            .critical
            .iter()
            .any(|e| matches!(e, CriticalStateEffect::CancelTimer { id } if *id == no_answer_id)),
        "the live NoAnswer fiber is cancelled, got {:?}",
        result.effects.critical,
    );
}

#[test]
fn in_dialog_bye_selects_relay_bye() {
    let call = test_call();
    // An in-dialog BYE (carries a To-tag) on the active call.
    let bye = in_dialog_request(sip_message::Method::Bye);
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(bye)),
        src: "127.0.0.1:5060".parse().unwrap(),
        matched_client_txn: false,
    };
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("relay-bye"));
}

/// An in-dialog request on a call whose BYE is in flight selects `post-bye-481`
/// (the session is terminated, §15.1.2 — never `relay-reinvite`), while ACK
/// still selects `relay-ack` and an OPTIONS liveness probe keeps
/// `relay-options` (RFC 3261 §11.2 — the dialog exists until the BYE completes).
#[test]
fn in_dialog_request_on_terminating_call_selects_post_bye_481() {
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    let config = B2buaConfig::default();
    for (method, expected) in [
        (sip_message::Method::Invite, "post-bye-481"),
        (sip_message::Method::Update, "post-bye-481"),
        (sip_message::Method::Info, "post-bye-481"),
        (sip_message::Method::Options, "relay-options"),
        (sip_message::Method::Ack, "relay-ack"),
    ] {
        let req = in_dialog_request(method.clone());
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(req)),
            src: "127.0.0.1:5060".parse().unwrap(),
            matched_client_txn: false,
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
        let rules = default_rules();
        let ranked = pick_ranked(&rules, &call, &ctx);
        assert_eq!(ranked.first().map(|r| r.id), Some(expected), "method {method:?}");
    }
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
                && matches!(&e.body, b2bua::effects::OutboundBody::Response(r) if r.status() == 503)
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
    let effect = b2bua::rules::relay::response_to_a_leg(
        &mut result.call,
        &mut result.effects,
        &a_invite,
        486,
        "Busy Here",
        Some("totag-x".into()),
        None,
        vec![],
        None,
        None,
        vec![],
    )
    .expect("the transaction's first final is admitted");
    result.effects.outbound.push(effect);
    assert_eq!(result.call.a_leg.invite_final_sent, Some(486), "the final is recorded on the leg");
    let result =
        invariants::enforce(&b2bua::obligations::ObligationSet::core(), &before, result, 0, true);
    let finals_to_a = result
        .effects
        .outbound
        .iter()
        .filter(|e| {
            e.leg_id.as_deref() == Some("a")
                && matches!(&e.body, b2bua::effects::OutboundBody::Response(r) if r.status() >= 200)
        })
        .count();
    assert_eq!(finals_to_a, 1, "exactly the rule's own final — no synthesized duplicate");
}

#[test]
fn no_synthesized_final_when_the_txn_layer_answered_the_cancel() {
    // The CANCEL path: sip-txn sent the 487 itself, so the a-leg reads Early
    // through the whole terminating window and the only trace of the final is
    // the leg fact the `Cancelled` turn recorded. The funnel reads that fact —
    // no per-turn scan, no CDR event — and answers nothing.
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    call = call::helpers::record_invite_final(call, "a", 487);
    let result = invariants::enforce(
        &b2bua::obligations::ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        true,
    );
    assert!(
        result.effects.outbound.is_empty(),
        "an a-leg carrying its 487 is not answered again: {:?}",
        result.effects.outbound
    );
    assert!(
        !result
            .call
            .cdr_events
            .iter()
            .any(|e| e.reason.as_deref() == Some("unanswered_at_termination")),
        "no synthesized-final CDR event either"
    );
    assert_eq!(result.call.a_leg.invite_final_sent, Some(487));
}

/// One executor turn, one Cancelled-shaped context, over `call`.
fn execute_on(call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
    };
    let config = B2buaConfig::default();
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config: &config,
        discharged: None,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    exec.execute(actions, call, &ctx)
}

fn finals_to_a(result: &HandlerResult) -> Vec<u16> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("a"))
        .filter_map(|e| match &e.body {
            b2bua::effects::OutboundBody::Response(r) if r.status() >= 200 => Some(r.status()),
            _ => None,
        })
        .collect()
}

fn refusals(result: &HandlerResult) -> Vec<(u16, u16)> {
    result
        .effects
        .buffered
        .iter()
        .filter_map(|e| match e {
            BufferedObservabilityEffect::SecondFinalRefused { status, carried } => {
                Some((*status, *carried))
            }
            _ => None,
        })
        .collect()
}

#[test]
fn a_second_final_to_the_a_leg_is_refused_and_reported() {
    // Two authored finals in one turn (a reject, then a relayed failure): the
    // a-leg seam admits the first, records it, and refuses the second — not
    // built, not pushed — reporting it for the `second_final_refused` count.
    let call = test_call();
    let reject = |status: u16, reason: &str| RuleAction::RespondToALeg {
        status,
        reason: reason.into(),
        header_updates: vec![],
        contacts: vec![],
    };
    let result = execute_on(
        &call,
        &[
            reject(486, "Busy Here"),
            reject(480, "Temporarily Unavailable"),
            RuleAction::RelayFailureToALeg { status: 503, reason: "Service Unavailable".into() },
        ],
    );
    assert_eq!(finals_to_a(&result), vec![486], "exactly the first final leaves");
    assert_eq!(result.call.a_leg.invite_final_sent, Some(486));
    assert_eq!(
        refusals(&result),
        vec![(480, 486), (503, 486)],
        "every later final is refused against the one the transaction carries"
    );
}

#[test]
fn a_final_after_the_autonomous_487_is_refused() {
    // The `Cancelled` turn recorded sip-txn's 487; a later rule answering the
    // cancelled call (a service timer, a relayed callee failure) is refused.
    let call = call::helpers::record_invite_final(test_call(), "a", 487);
    let result = execute_on(
        &call,
        &[RuleAction::RespondToALeg {
            status: 480,
            reason: "Temporarily Unavailable".into(),
            header_updates: vec![],
            contacts: vec![],
        }],
    );
    assert!(finals_to_a(&result).is_empty(), "nothing reaches the caller after the 487");
    assert_eq!(refusals(&result), vec![(480, 487)]);
    assert_eq!(result.call.a_leg.invite_final_sent, Some(487), "the 487 stands");
}

#[test]
fn a_provisional_is_not_refused_by_the_final_guard() {
    // The seam guards finals only; a relayed 18x on an unanswered leg passes
    // and leaves no fact behind.
    let call = test_call();
    let result = execute_on(
        &call,
        &[RuleAction::SendProvisionalToLeg {
            leg_id: "a".into(),
            status: 183,
            reason: "Session Progress".into(),
            body: vec![],
            content_type: None,
            to_tag: None,
            p_early_media: None,
        }],
    );
    let provisionals = result
        .effects
        .outbound
        .iter()
        .filter(
            |e| matches!(&e.body, b2bua::effects::OutboundBody::Response(r) if r.status() == 183),
        )
        .count();
    assert_eq!(provisionals, 1, "the 183 leaves: {:?}", result.effects.outbound);
    assert_eq!(result.call.a_leg.invite_final_sent, None, "a provisional records no final");
    assert!(refusals(&result).is_empty());
}

#[test]
fn begin_termination_resolves_an_a_leg_whose_invite_carries_its_final() {
    // The `Cancelled` turn: the a-leg is still Early (sip-txn sent the 487,
    // no TU response moved it) but its INVITE carries that final, so
    // `BeginTermination` records it Terminated — resolved, never re-answered.
    let mut call = test_call();
    call.a_leg.state = LegState::Early;
    let call = call::helpers::record_invite_final(call, "a", 487);
    let result =
        execute_on(&call, &[RuleAction::BeginTermination { reason: Some("CANCEL".into()) }]);
    assert_eq!(result.call.a_leg.state, LegState::Terminated);
    assert_eq!(result.call.a_leg.bye_disposition, Some(call::ByeDisposition::None));

    // An a-leg with NO final yet stays Early: the ADR-0022 funnel still owes
    // that caller its 503 at `→ terminated`.
    let mut call = test_call();
    call.a_leg.state = LegState::Early;
    let result = execute_on(&call, &[RuleAction::BeginTermination { reason: Some("BYE".into()) }]);
    assert_eq!(result.call.a_leg.state, LegState::Early);
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
    RuleDefinition { effects, ..sm_rule(handle) }
}

/// Like [`sm_rule`] but with a custom declared `transitions` list.
fn sm_rule_with_transitions(
    handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    transitions: &'static [(StateLabel, StateLabel)],
) -> RuleDefinition {
    RuleDefinition { transitions, ..sm_rule(handle) }
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
        teardown: false,
    }
}

fn info_event() -> CallEvent {
    let info = in_dialog_request(sip_message::Method::Info);
    CallEvent::Sip {
        message: Box::new(SipMessage::Request(info)),
        src: "127.0.0.1:5060".parse().unwrap(),
        matched_client_txn: false,
    }
}

fn ctx_for<'a>(
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
        now_ms: 0,
        config,
        discharged: None,
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
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
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
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
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
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
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
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
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
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
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
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
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
            pending_reinvite_2xx: None,
            answered_2xx: None,
            emitted_ack: None,
            awaited_ack_cseq: None,
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
        invite_final_sent: None,
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
        matched_client_txn: true,
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
        discharged: None,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };

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
        matched_client_txn: true,
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
        discharged: None,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };

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
        matched_client_txn: true,
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
        discharged: None,
    };
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };

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

    let config = B2buaConfig {
        b2b_outbound_proxy: Some(("proxy.example".to_string(), 5060)),
        ..B2buaConfig::default()
    };

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
        &CapabilitySet::default(),
        None, // no charging vector
        &[],
        &[], // no withheld option tags
        None,
    )
    .expect("no identity rewrites, so nothing to refuse");
    // Sanity: the topology is via-LB — the INVITE itself went to the proxy.
    assert_eq!(
        invite_effect.destination,
        ("proxy.example".to_string(), 5060),
        "b-leg INVITE egresses through the front proxy"
    );
    let invite_via = match &invite_effect.body {
        OutboundBody::Request(r) => {
            r.raw(HeaderName::Via).next().expect("INVITE has a Via").to_string()
        }
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
        discharged: None,
    };
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let result = exec.execute(&[RuleAction::CancelLeg { leg_id: "b-1".into() }], &call, &ctx);

    let cancel_effect = result
        .effects
        .outbound
        .iter()
        .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "CANCEL"))
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
    let routes: Vec<&str> = cancel.raw(HeaderName::Route).collect();
    assert_eq!(
        routes,
        vec!["<sip:proxy.example:5060;lr>"],
        "CANCEL must echo the INVITE's preloaded outbound-proxy Route (RFC 3261 §9.1)"
    );
    // ... and the transaction-correlation Via branch is the INVITE's verbatim.
    let cancel_via = cancel.raw(HeaderName::Via).next().expect("CANCEL has a Via").to_string();
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
    use sip_message::HeaderName;

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
                pending_reinvite_2xx: None,
                answered_2xx: None,
                emitted_ack: None,
                awaited_ack_cseq: None,
            },
        }];
    }

    /// An in-dialog INFO request (carries a To-tag) with a DTMF payload.
    fn in_dialog_info() -> SipRequest {
        let invite = super::invite();
        invite
            .thaw()
            .with_method(Method::Info)
            .set(invite.to().clone().with_tag("svc"))
            .set(sip_message::header::CSeq::new(2, Method::Info))
            .body(
                sip_message::Bytes::from_static(b"Signal=5\r\nDuration=160\r\n"),
                sip_message::header::MediaType::new("application/dtmf-relay"),
            )
            .freeze()
            .expect("an in-dialog INFO of the same dialog is complete")
    }

    fn exec_on<'a>(
        call: &'a call::Call,
        event: &'a CallEvent,
        source_leg_id: &'a str,
        config: &'a B2buaConfig,
        id_gen: &'a IdGen,
        actions: &[RuleAction],
    ) -> HandlerResult {
        let exec = ActionExecutor {
            config,
            id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let ctx = RuleContext {
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event,
            source_leg_id,
            direction: Direction::FromB,
            now_ms: 0,
            config,
            discharged: None,
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
            matched_client_txn: false,
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
            matched_client_txn: false,
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
            matched_client_txn: false,
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
                assert_eq!(r.method(), "INFO");
                assert_eq!(&r.body()[..], &mscml[..], "MSCML body passes through opaquely");
                assert_eq!(
                    r.raw(HeaderName::ContentType).next(),
                    Some("application/mediaservercontrol+xml")
                );
            }
            _ => panic!("expected an outbound request"),
        }
    }

    // A service re-originating an in-dialog request forwards arbitrary application
    // headers verbatim onto the request — the seam a deferred INFO_UUI RELAY uses
    // to carry a held `User-To-User` toward the peer at an async decision re-entry
    //. Body-owned headers (Content-Type/Content-Length) listed
    // here are dropped, never duplicated: `body`/`content_type` own those.
    #[test]
    fn send_request_to_leg_forwards_arbitrary_headers() {
        let mut call = test_call();
        call = call::helpers::add_b_leg(call, confirmed_b_leg("b-1", LegKind::Media));
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
            matched_client_txn: false,
        };
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let uui = "3030373946313233;encoding=hex"; // RFC 7433 User-To-User
        let body = b"SUP:example-binary\x00\x01payload".to_vec();
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
                content_type: Some("application/example-binary".into()),
                headers: vec![
                    ("User-To-User".into(), uui.into()),
                    ("X-Example-Trace".into(), "abc-123".into()),
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
                assert_eq!(r.method(), "INFO");
                assert_eq!(&r.body()[..], &body[..], "opaque body passes through unchanged");
                // Forwarded application headers survive verbatim.
                assert_eq!(r.raw(HeaderName::from("user-to-user")).next(), Some(uui));
                assert_eq!(r.raw(HeaderName::from("x-example-trace")).next(), Some("abc-123"));
                // Content-Type is owned by `content_type`: exactly one, from the
                // body — NOT the bogus forwarded one (dedup guard).
                let cts: Vec<&str> = r.raw(HeaderName::ContentType).collect();
                assert_eq!(
                    cts,
                    vec!["application/example-binary"],
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
            matched_client_txn: false,
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
                assert_eq!(r.status(), 183);
                assert_eq!(&r.body()[..], &sdp[..], "the MRF SDP is brokered onto A");
                assert!(r.to().tag().is_some(), "183 carries a B2BUA-minted early to-tag");
                assert_eq!(
                    r.raw(HeaderName::ContentType).next(),
                    Some("application/sdp"),
                    "an SDP body defaults to application/sdp"
                );
                assert_eq!(r.raw(HeaderName::PEarlyMedia).next(), Some("sendrecv"));
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
            matched_client_txn: false,
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
// relays the callee 200/SDP under A2, and confirms the a-leg (GAP-M-A / a
// MGIT spine).
mod answer_a_leg_new_dialog {
    use super::*;
    use b2bua::effects::OutboundBody;
    use b2bua::rules::RelayedFinal;
    use sip_message::generators::SourceBody;
    use sip_message::HeaderName;

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
                pending_reinvite_2xx: None,
                answered_2xx: None,
                emitted_ack: None,
                awaited_ack_cseq: None,
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
            matched_client_txn: false,
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
        let exec = ActionExecutor {
            config,
            id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let ctx = RuleContext {
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event,
            source_leg_id,
            direction: Direction::FromB,
            now_ms: 0,
            config,
            discharged: None,
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
                relayed: RelayedFinal::none(),
            }],
        );
        assert_eq!(result.effects.outbound.len(), 1);
        let eff = &result.effects.outbound[0];
        assert_eq!(eff.leg_id.as_deref(), Some("a"));
        let a2 = match &eff.body {
            OutboundBody::Response(r) => {
                assert_eq!(r.status(), 200);
                assert_eq!(&r.body()[..], &sdp_b[..], "the callee SDP-B rides the 200");
                assert_eq!(
                    r.raw(HeaderName::ContentType).next(),
                    Some("application/sdp"),
                    "an SDP body defaults to application/sdp"
                );
                let tag =
                    r.to().tag().map(str::to_owned).expect("the 200 carries an a-facing To-tag");
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

    /// Answering the caller ends the setup's reliable provisionals and nothing
    /// else: a b-leg's re-INVITE 2xx still awaiting its ACK keeps its ladder,
    /// its give-up and its retained marker (RFC 3261 §13.3.1.4; the RFC 6026
    /// *Accepted* marker `reinvite-glare` reads), while the caller-facing §3
    /// ladder is retired with the final (§17.2.1).
    #[test]
    fn answering_the_caller_leaves_a_b_legs_pending_reinvite_2xx_ladder_alone() {
        let mut call = call_with_early_a_dialog("A1early");
        let mut b = super::b_leg_pending();
        b.state = LegState::Confirmed;
        b.dialogs[0].ext.pending_reinvite_2xx = Some(call::Unacked2xx {
            dialog_tag: "svc".into(),
            cseq: 7,
            emission: call::RetainedEmission::paced(
                b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
                ("10.0.0.2".into(), 5070),
                sip_retransmit::Class::Final2xx,
                call::Repeated::response("INVITE", 200),
            )
            .0,
        });
        call = call::helpers::add_b_leg(call, b);
        let reinvite_2xx =
            call::Obligation::AckOf2xx { leg: "b-1".into(), dialog_tag: "svc".into(), cseq: 7 };
        call.reliable_provisionals.push(call::ReliableProvisional {
            a_tag: "A1early".into(),
            a_rseq: 1,
            a_cseq: Some(1),
            b_leg_id: "b-1".into(),
            b_tag: "svc".into(),
            b_cseq: 1,
            b_rseq: 9,
            acknowledged: false,
            emission: None,
        });
        let provisional = call::Obligation::PrackOf { a_tag: "A1early".into(), a_rseq: 1 };
        for (timer_type, fire_at) in [
            (TimerType::Rung { obligation: reinvite_2xx.clone() }, 500),
            (TimerType::RepeatGiveUp { obligation: reinvite_2xx.clone() }, 32_000),
            (TimerType::Rung { obligation: provisional.clone() }, 500),
            (TimerType::RepeatGiveUp { obligation: provisional.clone() }, 32_000),
        ] {
            call.timers.push(call::TimerEntry {
                id: timer_type.timer_id(None),
                timer_type,
                fire_at,
                leg_id: None,
            });
        }

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
                to_tag: None,
                header_updates: vec![],
                relayed: RelayedFinal::none(),
            }],
        );

        let b_dialog = &result.call.b_legs[0].dialogs[0];
        assert!(
            b_dialog.ext.pending_reinvite_2xx.is_some(),
            "the b-leg's re-INVITE 2xx still awaits its ACK: marker kept",
        );
        let ladder_timers = |o: &call::Obligation| {
            result
                .call
                .timers
                .iter()
                .filter(|t| matches!(&t.timer_type,
                    TimerType::Rung { obligation } | TimerType::RepeatGiveUp { obligation } if obligation == o))
                .count()
        };
        assert_eq!(
            ladder_timers(&reinvite_2xx),
            2,
            "the b-leg 2xx's rung and give-up stay in the ledger"
        );
        assert_eq!(
            ladder_timers(&provisional),
            0,
            "the caller-facing §3 ladder ends with the final"
        );
        assert!(
            result.call.reliable_provisionals.is_empty()
                || result.call.reliable_provisionals[0].emission.is_none(),
            "the provisional repeats nothing more",
        );
        let cancelled: Vec<&str> = result
            .effects
            .critical
            .iter()
            .filter_map(|e| match e {
                CriticalStateEffect::CancelTimer { id } => Some(id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            cancelled,
            vec!["Rung:PrackOf:A1early:1", "RepeatGiveUp:PrackOf:A1early:1"],
            "only the provisional's timers leave the driver",
        );
        assert!(
            result.call.a_leg.dialogs[0].ext.answered_2xx.is_some(),
            "the caller's answer is retained under its own obligation",
        );
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
                relayed: RelayedFinal::none(),
            }],
        );
        let eff = &result.effects.outbound[0];
        match &eff.body {
            OutboundBody::Response(r) => {
                assert_eq!(r.to().tag(), Some("A2explicit"), "the supplied A2 is used verbatim");
                assert_eq!(r.raw(HeaderName::from("x-served-by")).next(), Some("mrf"));
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
                relayed: RelayedFinal::none(),
            }],
        );
        assert!(result.effects.outbound.is_empty(), "a non-2xx final establishes no a-dialog");
        assert_eq!(
            result.call.a_leg.dialogs.first().map(|d| d.sip.local_tag.as_str()),
            Some("A1early"),
            "the early dialog A1 is left untouched",
        );
    }

    // The fork-confirm 2xx is an a-facing final the B2BUA answers on a leg of
    // its own: with no callee response to relay an advertisement from and no
    // declaration, it states none (issue 174).
    #[test]
    fn states_no_capability_advert_unless_declared() {
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
                to_tag: None,
                header_updates: vec![],
                relayed: RelayedFinal::none(),
            }],
        );
        match &result.effects.outbound[0].body {
            OutboundBody::Response(r) => {
                for name in [HeaderName::Allow, HeaderName::Supported, HeaderName::Accept] {
                    assert_eq!(
                        r.raw(name.clone()).count(),
                        0,
                        "no {} on a 2xx nobody declared a set for",
                        name.as_wire_str()
                    );
                }
            }
            _ => panic!("expected an outbound response"),
        }
    }

    // A header_updates entry naming Allow/Supported owns it: a set value
    // replaces the resolved advert verbatim (exactly once), a removal keeps the header
    // absent — the pre-026 downstream workaround stays valid.
    #[test]
    fn header_updates_override_the_capability_advert() {
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
                to_tag: None,
                header_updates: vec![
                    ("Supported".into(), Some("timer".into())),
                    ("Allow".into(), None),
                ],
                relayed: RelayedFinal::none(),
            }],
        );
        match &result.effects.outbound[0].body {
            OutboundBody::Response(r) => {
                assert_eq!(
                    r.raw(HeaderName::Supported).next(),
                    Some("timer"),
                    "a set value replaces the resolved advert verbatim"
                );
                let n = r.raw(HeaderName::Supported).count();
                assert_eq!(n, 1, "the service value is not duplicated by the default");
                assert_eq!(
                    r.raw(HeaderName::Allow).next(),
                    None,
                    "an explicit removal keeps the header absent"
                );
            }
            _ => panic!("expected an outbound response"),
        }
    }
    /// The callee final this answer delivers, stating privacy, an identity line
    /// of its own, a vendor header, its capability advert, and lines the stack
    /// owns or withholds.
    fn callee_final() -> sip_message::SipResponse {
        let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.70:5060;branch=z9hG4bK-b\r\n\
From: <sip:alice@host>;tag=alice-tag\r\n\
To: <sip:callee@host>;tag=callee-tag\r\n\
Call-ID: b-leg\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:callee@10.0.0.70:5060>\r\n\
Privacy: none\r\n\
P-Identifier: 112233368\r\n\
X-Vendor-Thing: opaque-42\r\n\
Allow: INVITE, ACK, BYE\r\n\
Session-Expires: 1800;refresher=uas\r\n\
Content-Type: application/sdp\r\n\
Content-Length: 4\r\n\r\nv=0\n";
        match CustomParser::new().parse(raw.as_bytes()).unwrap() {
            SipMessage::Response(r) => r,
            _ => panic!("expected a response"),
        }
    }

    /// RFC 3261 §16.6: the answer delivering a callee final carries that final's
    /// relayable lines — and only those — the way the plain relay would: the
    /// structural set, the callee's Contact and the per-leg negotiation stay
    /// behind, and the callee's advert is the caller's to see where the face
    /// declares none.
    #[test]
    fn relays_the_delivered_final_onto_the_answer() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let delivered = callee_final();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: delivered.body().to_vec(),
                content_type: None,
                to_tag: None,
                header_updates: vec![],
                relayed: RelayedFinal::of(&delivered, SourceBody::Verbatim),
            }],
        );
        match &result.effects.outbound[0].body {
            OutboundBody::Response(r) => {
                assert_eq!(r.raw(HeaderName::from("Privacy")).next(), Some("none"));
                assert_eq!(r.raw(HeaderName::from("P-Identifier")).next(), Some("112233368"));
                assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).next(), Some("opaque-42"));
                assert_eq!(
                    r.raw(HeaderName::Allow).next(),
                    Some("INVITE, ACK, BYE"),
                    "an undeclared half states what the delivered final advertised"
                );
                assert_eq!(r.raw(HeaderName::Allow).count(), 1);
                assert_eq!(
                    r.raw(HeaderName::SessionExpires).next(),
                    None,
                    "per-leg negotiation is withheld"
                );
                assert_eq!(
                    r.raw(HeaderName::Via).count(),
                    1,
                    "the caller's own Via, not the callee's"
                );
                assert_eq!(
                    r.raw(HeaderName::Contact).count(),
                    1,
                    "the B2BUA's Contact, not the callee's"
                );
                assert_eq!(r.to().tag(), Some(result.call.a_leg.dialogs[0].sip.local_tag.as_str()));
            }
            _ => panic!("expected an outbound response"),
        }
    }

    /// A `header_updates` entry naming a header owns it over the relayed line of
    /// the same name — a set value replaces it, a removal keeps it absent.
    #[test]
    fn header_updates_own_a_name_over_the_relayed_line() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let delivered = callee_final();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: delivered.body().to_vec(),
                content_type: None,
                to_tag: None,
                header_updates: vec![
                    ("X-Vendor-Thing".into(), Some("service-owned".into())),
                    ("P-Identifier".into(), None),
                    ("Allow".into(), Some("INVITE".into())),
                ],
                relayed: RelayedFinal::of(&delivered, SourceBody::Verbatim),
            }],
        );
        match &result.effects.outbound[0].body {
            OutboundBody::Response(r) => {
                assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).next(), Some("service-owned"));
                assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).count(), 1);
                assert_eq!(
                    r.raw(HeaderName::from("P-Identifier")).next(),
                    None,
                    "a removal keeps the relayed line off"
                );
                assert_eq!(r.raw(HeaderName::Allow).next(), Some("INVITE"));
                assert_eq!(r.raw(HeaderName::Allow).count(), 1);
                assert_eq!(
                    r.raw(HeaderName::from("Privacy")).next(),
                    Some("none"),
                    "the rest still rides"
                );
            }
            _ => panic!("expected an outbound response"),
        }
    }
}

// ── AckLeg body + Content-Type (RFC 3261 §13.2.2.4 delayed-offer answer) ─────
//
// `RuleAction::AckLeg` carries an optional body so a rule can ACK a 2xx with an
// SDP answer, completing a delayed-offer exchange (RFC 3264 §4 — the answer
// rides the ACK). A body-bearing ACK defaults its Content-Type to
// `application/sdp`; an explicit type overrides it; an empty body stays a bare
// ACK (no body, no Content-Type — the pre-body behaviour, unchanged). The body
// is a binary-safe `Vec<u8>`. Every emitted ACK retains its exact datagram on
// the dialog (`emitted_ack`), and a bodyless AckLeg re-passes those bytes raw
// (§13.2.2.4 — the re-ACK is THE ACK the 2xx triggered).
mod ack_leg_body {
    use super::*;
    use b2bua::effects::OutboundBody;
    use sip_message::HeaderName;

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
            discharged: None,
        };
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let result = exec.execute(
            &[RuleAction::AckLeg { leg_id: "b-1".into(), body, content_type }],
            &call,
            &ctx,
        );
        let effect = result
            .effects
            .outbound
            .iter()
            .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "ACK"))
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
        let mut body = b"v=0\r\no=- 0 0 IN IP4 10.0.0.9\r\ns=-\r\nc=IN IP4 10.0.0.9\r\nt=0 0\r\n\
              m=audio 40000 RTP/AVP 0\r\n"
            .to_vec();
        body.push(0xFF); // a non-UTF-8 byte must survive the Vec<u8> round-trip.

        let ack = ack_request(body.clone(), None);
        assert_eq!(
            &ack.body()[..],
            &body[..],
            "the delayed-offer answer rides the ACK byte-for-byte (binary-safe)"
        );
        assert_eq!(
            ack.raw(HeaderName::ContentType).next(),
            Some("application/sdp"),
            "a body-bearing ACK defaults Content-Type to application/sdp (§13.2.2.4)"
        );
    }

    // An explicit content_type overrides the application/sdp default.
    #[test]
    fn ack_leg_honours_explicit_content_type_override() {
        let body = b"<some>opaque</some>".to_vec();
        let ack = ack_request(body.clone(), Some("application/custom".to_string()));
        assert_eq!(&ack.body()[..], &body[..]);
        assert_eq!(
            ack.raw(HeaderName::ContentType).next(),
            Some("application/custom"),
            "an explicit content_type is used verbatim (not coerced to application/sdp)"
        );
    }

    // Regression: an empty body sends a bare ACK with no Content-Type — the
    // behaviour before AckLeg grew a body, unchanged.
    #[test]
    fn ack_leg_empty_body_sends_bare_ack_with_no_content_type() {
        let ack = ack_request(Vec::new(), None);
        assert!(ack.body().is_empty(), "an empty AckLeg body sends a bodyless ACK");
        assert_eq!(
            ack.raw(HeaderName::ContentType).next(),
            None,
            "a bare ACK carries no Content-Type (no regression vs the pre-body AckLeg)"
        );
    }

    // ── The retained ACK datagram (§13.2.2.4 re-ACK: THE ack, re-passed) ─────

    /// Execute one `AckLeg { leg_id: "b-1", body, content_type }` against `call`
    /// and return the full [`HandlerResult`] (these tests read both the wire
    /// effect and the updated call).
    fn ack_exec(call: &call::Call, body: Vec<u8>, content_type: Option<String>) -> HandlerResult {
        let config = B2buaConfig::default();
        let event = CallEvent::Timer {
            timer_type: TimerType::NoAnswer,
            call_ref: call.call_ref.clone(),
            leg_id: Some("b-1".to_string()),
        };
        let ctx = RuleContext {
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event: &event,
            source_leg_id: "b-1",
            direction: Direction::FromB,
            now_ms: 0,
            config: &config,
            discharged: None,
        };
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        exec.execute(&[RuleAction::AckLeg { leg_id: "b-1".into(), body, content_type }], call, &ctx)
    }

    /// The one emitted ACK effect in `result`.
    fn ack_effect(result: &HandlerResult) -> &b2bua::effects::OutboundSipEffect {
        result
            .effects
            .outbound
            .iter()
            .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "ACK"))
            .expect("AckLeg emits an ACK request")
    }

    /// The one re-ACK in `result`: a retained datagram repeated as its bytes
    /// (RFC 3261 §13.2.2.4), never a composed request.
    fn re_ack_effect(result: &HandlerResult) -> &b2bua::effects::OutboundSipEffect {
        assert!(
            !result.effects.outbound.iter().any(|e| matches!(&e.body, OutboundBody::Request(_))),
            "a re-ACK composes nothing: {:?}",
            result.effects.outbound
        );
        result
            .effects
            .outbound
            .iter()
            .find(|e| matches!(&e.body, OutboundBody::Datagram(_)))
            .expect("the re-ACK repeats the retained datagram")
    }

    // A body-bearing ACK (the relayed delayed-offer answer) retains its exact
    // wire bytes + destination on the dialog, so a §13.2.2.4 re-ACK can re-pass
    // THE ACK instead of composing an answerless one.
    #[test]
    fn a_body_bearing_ack_retains_its_exact_datagram_and_destination() {
        let call = call::helpers::add_b_leg(test_call(), b_leg_confirmed());
        let result = ack_exec(&call, b"answer-sdp".to_vec(), None);
        let effect = ack_effect(&result);
        let OutboundBody::Request(sent) = &effect.body else { unreachable!() };
        let retained = result.call.b_legs[0]
            .dialogs
            .first()
            .and_then(|d| d.ext.emitted_ack.clone())
            .expect("the emitted ACK datagram is retained on the dialog");
        let (bytes, dest) = retained.wire();
        assert_eq!(bytes, sent.image(), "retained bytes are the wire bytes");
        assert_eq!(
            dest,
            (effect.destination.0.as_str(), effect.destination.1),
            "retained destination is the wire destination"
        );
        assert_eq!(
            retained.repeat(),
            call::Repeat::OnTrigger,
            "an ACK is repeated only when a 2xx copy provokes it"
        );
    }

    // RFC 3261 §13.2.2.4: with a datagram retained, a bodyless AckLeg (the
    // `re-ack-retransmitted-2xx` action) re-passes THE ACK raw — byte-identical,
    // answer included, at the retained destination — never a fresh bodyless one.
    #[test]
    fn a_bodyless_ack_re_passes_the_retained_datagram_byte_identical() {
        let call = call::helpers::add_b_leg(test_call(), b_leg_confirmed());
        let first = ack_exec(&call, b"answer-sdp".to_vec(), None);
        let first_effect = ack_effect(&first);
        let OutboundBody::Request(sent) = &first_effect.body else { unreachable!() };
        let first_bytes = sent.image().to_vec();

        let second = ack_exec(&first.call, Vec::new(), None);
        let effect = re_ack_effect(&second);
        let OutboundBody::Datagram(re_sent) = &effect.body else { unreachable!() };
        assert_eq!(
            re_sent.wire().0,
            &first_bytes[..],
            "the re-ACK is the SAME datagram, answer included"
        );
        assert_eq!(
            (re_sent.repeat().ladder(), re_sent.repeated().method(), re_sent.repeated().code()),
            ("trigger", "ACK", None),
            "the re-ACK leaves labelled as a triggered repeat of an ACK",
        );
        let re_sent = re_sent.wire().0;
        assert_eq!(effect.destination, first_effect.destination, "same wire destination");
        assert!(
            re_sent.ends_with(b"answer-sdp"),
            "the delayed-offer answer rides the re-ACK (a fresh composition would drop it)"
        );
    }

    // The minted, bodyless first ACK retains its datagram too: the §13.2.2.4
    // unit is the datagram regardless of body, so a copy of that 2xx draws the
    // same bare bytes back.
    #[test]
    fn a_bare_first_ack_retains_its_datagram_and_a_re_ack_repeats_it() {
        let call = call::helpers::add_b_leg(test_call(), b_leg_confirmed());
        let first = ack_exec(&call, Vec::new(), None);
        let OutboundBody::Request(sent) = &ack_effect(&first).body else { unreachable!() };
        let retained = first.call.b_legs[0]
            .dialogs
            .first()
            .and_then(|d| d.ext.emitted_ack.clone())
            .expect("the bare ACK is retained as well");
        assert_eq!(retained.wire().0, sent.image());
        assert!(sent.body().is_empty(), "a minted ACK stays bare");

        let second = ack_exec(&first.call, Vec::new(), None);
        let OutboundBody::Datagram(re_sent) = &re_ack_effect(&second).body else { unreachable!() };
        assert_eq!(re_sent.wire().0, sent.image(), "the re-ACK repeats the bare datagram");
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
            matched_client_txn: false,
        };
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config,
            id_gen: &id_gen,
            now_ms: 1_700_000_000_000,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let ctx = RuleContext {
            call: RuleCall::new(&call),
            call_ref: &call.call_ref,
            event: &event,
            source_leg_id: "a",
            direction: Direction::FromA,
            now_ms: 1_700_000_000_000,
            config,
            discharged: None,
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
        assert!(result.call.b_legs.is_empty(), "no b-leg state is allocated on admission reject");
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
        let config =
            B2buaConfig { worker_allowed_target_suffixes: vec!["*".into()], ..Default::default() };
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
    use sip_message::HeaderName;

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
            matched_client_txn: false,
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
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };

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
            .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "INVITE"))
            .expect("CreateLeg emits a b-leg INVITE");
        let inv = match &invite_effect.body {
            OutboundBody::Request(r) => r,
            _ => unreachable!(),
        };
        assert_eq!(
            &inv.body()[..],
            &sdp[..],
            "the b-leg INVITE carries the config default_sdp sourced via body_override"
        );
        assert_eq!(
            inv.raw(HeaderName::ContentType).next(),
            Some("application/sdp"),
            "the fake-offer INVITE advertises Content-Type: application/sdp"
        );
    }
}

// ── header_updates removal beats the §16.6 relay on an originated request ───
//
// `(name, None)` states that a name does not ride. On a response the mint sites
// already honour it; on an originated request the §16.6 relay would otherwise
// re-add the originator's own copy, so a caller asking for a header to be
// withheld would see it travel anyway. The removal owns the name on every path.
mod header_update_removal_withholds_a_relayed_name {
    use super::*;
    use b2bua::effects::OutboundBody;
    use sip_message::{HeaderName, SipHeader};

    /// An a-leg INVITE carrying a vendor annotation the originator sent.
    fn invite_with_vendor_header() -> SipRequest {
        let opts = GenerateOutOfDialogRequestOpts {
            request_uri: Some(uri_of("sip:bob@127.0.0.1:5070")),
            call_id: "c1@alice".into(),
            from: Some(
                header::From::from_uri(uri_of("sip:alice@host"))
                    .with_tag(SipStr::from_static("atag")),
            ),
            to: Some(header::To::from_uri(uri_of("sip:bob@host"))),
            cseq: 1,
            via: Some(Via::udp("127.0.0.1", 5060).with_branch(SipStr::from_static("z9hG4bKalice"))),
            contact: Some(header::Contact::from_uri(
                Uri::sip_user("alice", "127.0.0.1").with_port(5060),
            )),
            max_forwards: Some(70),
            body: b"v=0\r\n".to_vec(),
            content_type: None,
            extra_headers: vec![
                SipHeader { name: "P-Term".into(), value: "sbc.example".into() },
                SipHeader { name: "P-Kept".into(), value: "rides-on".into() },
            ],
        };
        generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts)
    }

    /// The b-leg INVITE a `CreateLeg` carrying `header_updates` emits.
    fn b_leg_invite(header_updates: Vec<(String, Option<String>)>) -> SipRequest {
        let config = B2buaConfig::default();
        let a_invite = invite_with_vendor_header();
        let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let call = build_initial_call(&a_invite, src, &config, 0);
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(a_invite)),
            src,
            matched_client_txn: false,
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
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let create = RuleAction::CreateLeg {
            destination: ("10.0.1.5".into(), 5070), // IP literal → admission passes
            new_ruri: None,
            new_from: None,
            new_to: None,
            no_answer_timeout_sec: None,
            callback_context: None,
            body_override: None,
            header_updates,
            kind: None,
        };
        let result = exec.execute(&[create], &call, &ctx);
        match &result
            .effects
            .outbound
            .iter()
            .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "INVITE"))
            .expect("CreateLeg emits a b-leg INVITE")
            .body
        {
            OutboundBody::Request(r) => r.clone(),
            _ => unreachable!(),
        }
    }

    /// Without a removal the annotation rides on — the §16.6 default this test
    /// exists to keep honest, so the removal case cannot pass vacuously.
    #[test]
    fn the_relay_carries_the_name_when_nothing_removes_it() {
        let inv = b_leg_invite(vec![]);
        assert_eq!(inv.raw(HeaderName::from("P-Term")).next(), Some("sbc.example"));
        assert_eq!(inv.raw(HeaderName::from("P-Kept")).next(), Some("rides-on"));
    }

    #[test]
    fn a_removal_withholds_the_name_and_leaves_every_other_relayed_header() {
        let inv = b_leg_invite(vec![("P-Term".into(), None)]);
        assert_eq!(
            inv.raw(HeaderName::from("P-Term")).next(),
            None,
            "a removal beats the relayed copy of the same name"
        );
        assert_eq!(
            inv.raw(HeaderName::from("P-Kept")).next(),
            Some("rides-on"),
            "and withholds nothing else"
        );
    }

    /// Case-insensitively, since a name is a header name and not a string.
    #[test]
    fn the_removal_matches_the_name_however_it_is_spelled() {
        let inv = b_leg_invite(vec![("p-TERM".into(), None)]);
        assert_eq!(inv.raw(HeaderName::from("P-Term")).next(), None);
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

        result.effects.critical.retain(|e| !matches!(e, CriticalStateEffect::RemoveCall));
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

// ── Service-owned timers (TimerType::Service) ────────────────────────────────
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
        RuleAction::ScheduleTimer { timer_type: t, delay: TimerDelay::secs(secs), leg_id: None }
    }

    #[test]
    fn distinct_keys_coexist_and_same_key_reschedule_supersedes() {
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 1_000,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let call = test_call();
        let event = info_like_event(&call);
        let ctx = ctx_at(&call, &event, &config);

        let fast = TimerType::service(SVC, "fast");
        let slow = TimerType::service(SVC, "slow");
        let result =
            exec.execute(&[schedule(fast.clone(), 3), schedule(slow.clone(), 6)], &call, &ctx);
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
            discharged: None,
        }
    }
}

// ── Declared capability advertisement: call model → wire ────────────────────

/// A call declaring a narrow set toward the originated leg and the full set
/// toward the originator (`advertise_capabilities`).
fn call_declaring_asymmetric_capabilities() -> call::Call {
    let mut call = test_call();
    let mut features = b2bua::decision::default_platform_features();
    features.advertise_capabilities = Some(call::features::AdvertiseCapabilitiesFeature {
        toward_originator: None,
        toward_originated: Some(call::features::AdvertisedCapabilities {
            allow: Some(["INVITE", "ACK", "CANCEL", "BYE"].iter().map(|s| s.to_string()).collect()),
            supported: Some(vec!["timer".to_string()]),
        }),
    });
    call.features = Some(features);
    call
}

/// The declared set reaches the originated leg's wire header, resolved off the
/// call's own feature activations — the model→mint-point→wire chain.
#[test]
fn a_declared_capability_set_reaches_the_originated_leg_wire_header() {
    let call = call_declaring_asymmetric_capabilities();
    let a_invite = b2bua::rules::relay::rebuild_a_leg_invite(&call.a_leg_invite);
    let (_leg, effect) = b2bua::rules::relay::build_b_leg(
        &call.call_ref,
        "b-1",
        false,
        &a_invite,
        ("10.0.0.2".to_string(), 5070),
        None,
        None,
        None,
        None,
        &B2buaConfig::default(),
        &IdGen::seeded(11),
        None,
        &[],
        &b2bua::rules::capabilities::for_leg(&call, "b-1"),
        None, // no charging vector
        &[],
        &[], // no withheld option tags
        None,
    )
    .expect("no identity rewrites, so nothing to refuse");
    let invite = match effect.body {
        b2bua::effects::OutboundBody::Request(r) => r,
        b2bua::effects::OutboundBody::Response(_) | b2bua::effects::OutboundBody::Datagram(_) => {
            panic!("b-leg effect must carry a request")
        }
    };
    let allow = invite.raw_text(HeaderName::Allow).next().map(|v| v.as_str().to_string());
    let supported = invite.raw_text(HeaderName::Supported).next().map(|v| v.as_str().to_string());
    assert_eq!(allow.as_deref(), Some("INVITE, ACK, CANCEL, BYE"));
    assert_eq!(supported.as_deref(), Some("timer"));
}

/// The undeclared face states nothing of its own, so one declaration cannot
/// reach the other side of the bridge.
#[test]
fn the_undeclared_face_of_a_declaring_call_states_nothing() {
    let call = call_declaring_asymmetric_capabilities();
    use b2bua::rules::capabilities::{self, Face};
    assert_eq!(capabilities::declared(&call, Face::Originator), None, "nothing declared here");
    assert_eq!(capabilities::for_leg(&call, "a"), CapabilitySet::silent());
    assert!(capabilities::declared(&call, Face::Originated).is_some());
    assert_ne!(capabilities::for_leg(&call, "b-1"), CapabilitySet::silent());
}

/// A call that declares nothing states no set of its own on either face.
#[test]
fn an_undeclared_call_states_nothing_on_every_face() {
    let call = test_call();
    assert_eq!(b2bua::rules::capabilities::for_leg(&call, "a"), CapabilitySet::silent());
    assert_eq!(b2bua::rules::capabilities::for_leg(&call, "b-1"), CapabilitySet::silent());
}

// ── the keepalive ledger ceiling (arming side) ────────────────────────────────

/// A `RuleContext` for `event` on `call` at `now_ms`, acting on the a-leg.
fn timer_ctx<'a>(
    call: &'a call::Call,
    event: &'a CallEvent,
    config: &'a B2buaConfig,
    now_ms: i64,
) -> RuleContext<'a> {
    RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms,
        config,
        discharged: None,
    }
}

/// The `keepalive` rule's own re-arm enters the ledger at exactly the configured
/// cadence — never farther — so no restored call can inherit a deadline the
/// restore seam has to clamp, and a policy deadline that legitimately outlives a
/// probe cadence (`GlobalDuration`) is untouched.
#[test]
fn the_keepalive_rule_arms_its_ledger_deadline_at_exactly_one_cadence() {
    let now_ms = 1_000_000;
    let config = B2buaConfig::default();
    let interval_ms = config.keepalive_interval_sec * 1000;
    let call = test_call();
    let event = CallEvent::Timer {
        timer_type: TimerType::Keepalive,
        call_ref: call.call_ref.clone(),
        leg_id: None,
    };
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    let keepalive = ranked.iter().find(|r| r.id == "keepalive").expect("keepalive is a candidate");
    let mut actions = (keepalive.handle)(&ctx).expect("keepalive handles its own timer").actions;
    actions.push(RuleAction::ScheduleTimer {
        timer_type: TimerType::GlobalDuration,
        delay: TimerDelay::secs(3600),
        leg_id: None,
    });

    let id_gen = IdGen::seeded(0x65);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let result = exec.execute(&actions, &call, &ctx);

    let armed = result
        .call
        .timers
        .iter()
        .find(|t| t.timer_type == TimerType::Keepalive)
        .expect("the rule re-arms the probe");
    assert_eq!(
        armed.fire_at,
        now_ms + interval_ms,
        "the probe is re-armed one cadence out, in this node's clock frame",
    );
    let duration = result
        .call
        .timers
        .iter()
        .find(|t| t.timer_type == TimerType::GlobalDuration)
        .expect("the policy timer is armed");
    assert_eq!(
        duration.fire_at,
        now_ms + 3_600_000,
        "a non-keepalive deadline keeps its full delay — the ceiling is keepalive-only",
    );
    let replicated = result
        .effects
        .critical
        .iter()
        .find_map(|e| match e {
            CriticalStateEffect::ScheduleTimer(t) if t.timer_type == TimerType::Keepalive => {
                Some(t)
            }
            _ => None,
        })
        .expect("the probe's ledger write is replicated");
    assert_eq!(
        replicated.fire_at, armed.fire_at,
        "the replicated copy a peer later hydrates carries the ledger's capped deadline",
    );
}

/// An arming site that computes a `Keepalive` deadline beyond one cadence is a
/// defect at that site: the ledger invariant trips in debug builds instead of
/// letting a foreign-frame deadline be persisted and replicated.
#[cfg(debug_assertions)]
#[test]
#[should_panic(expected = "Keepalive armed beyond one interval")]
fn arming_a_keepalive_beyond_one_cadence_trips_the_ledger_invariant() {
    let now_ms = 1_000_000;
    let config = B2buaConfig::default();
    let call = test_call();
    let event = CallEvent::Timer {
        timer_type: TimerType::Keepalive,
        call_ref: call.call_ref.clone(),
        leg_id: None,
    };
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let id_gen = IdGen::seeded(0x65);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    exec.execute(
        &[RuleAction::ScheduleTimer {
            timer_type: TimerType::Keepalive,
            delay: TimerDelay::secs(config.keepalive_interval_sec * 2),
            leg_id: None,
        }],
        &call,
        &ctx,
    );
}

/// In release the same over-cadence deadline is clamped rather than fatal: an
/// early probe costs nothing, a probe a cadence late costs the call its peer's
/// keepalive tolerance.
#[cfg(not(debug_assertions))]
#[test]
#[ignore = "release-profile clamp — slow lane (just test-slow)"]
fn arming_a_keepalive_beyond_one_cadence_is_clamped() {
    let now_ms = 1_000_000;
    let config = B2buaConfig::default();
    let call = test_call();
    let event = CallEvent::Timer {
        timer_type: TimerType::Keepalive,
        call_ref: call.call_ref.clone(),
        leg_id: None,
    };
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let id_gen = IdGen::seeded(0x65);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let result = exec.execute(
        &[RuleAction::ScheduleTimer {
            timer_type: TimerType::Keepalive,
            delay: TimerDelay::secs(config.keepalive_interval_sec * 2),
            leg_id: None,
        }],
        &call,
        &ctx,
    );
    let armed = result
        .call
        .timers
        .iter()
        .find(|t| t.timer_type == TimerType::Keepalive)
        .expect("the probe is armed");
    assert_eq!(
        armed.fire_at,
        now_ms + config.keepalive_interval_sec * 1000,
        "the over-cadence deadline is clamped to one interval out",
    );
}

// ── §13.3.1.4 answered-2xx discharge (the engine's, before the rules) ────────

/// A call whose a-leg dialog retains the initial-INVITE 2xx under the key the
/// caller's ACK carries (To-tag `btag`, CSeq 1), its ladder armed — the state
/// the engine must discharge.
fn call_with_retained_answered_2xx() -> call::Call {
    let mut call = test_call();
    call.a_leg.state = LegState::Confirmed;
    let mut b = b_leg_pending();
    b.state = LegState::Confirmed;
    call = call::helpers::add_b_leg(call, b);
    // The confirmed a-leg UAS dialog the answer seam retained the 2xx on.
    call.a_leg.dialogs.push(Dialog {
        sip: StackDialog {
            call_id: call.a_leg.call_id.clone(),
            local_tag: "btag".into(),
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
            pending_reinvite_2xx: None,
            awaited_ack_cseq: None,
            answered_2xx: Some(call::Unacked2xx {
                dialog_tag: "btag".into(),
                cseq: 1,
                emission: call::RetainedEmission::paced(
                    b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
                    ("127.0.0.1".into(), 5060),
                    sip_retransmit::Class::Final2xx,
                    call::Repeated::response("INVITE", 200),
                )
                .0,
            }),
            emitted_ack: None,
        },
    });
    let obligation = answered_obligation();
    for (timer_type, fire_at) in [
        (TimerType::Rung { obligation: obligation.clone() }, 500),
        (TimerType::RepeatGiveUp { obligation }, 32_000),
    ] {
        call.timers.push(call::TimerEntry {
            id: timer_type.timer_id(None),
            timer_type,
            fire_at,
            leg_id: None,
        });
    }
    call
}

/// The key the retained answer of [`call_with_retained_answered_2xx`] is owed
/// under.
fn answered_obligation() -> call::Obligation {
    call::Obligation::AckOf2xx { leg: "a".into(), dialog_tag: "btag".into(), cseq: 1 }
}

/// The engine's discharge step for an in-dialog ACK arriving on
/// `source_leg_id`: what it retired, the call it leaves, and its effects.
fn discharge_ack(
    call: &call::Call,
    source_leg_id: &str,
) -> (Option<call::Obligation>, call::Call, b2bua::effects::HandlerEffects) {
    let ack = in_dialog_request(Method::Ack);
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(ack)),
        src: "127.0.0.1:5060".parse().unwrap(),
        matched_client_txn: false,
    };
    let config = B2buaConfig::default();
    let id_gen = IdGen::seeded(1);
    let exec = ActionExecutor {
        config: &config,
        id_gen: &id_gen,
        now_ms: 0,
        wire_faults: &b2bua::wire_faults::WireFaults::none(),
    };
    let mut call = call.clone();
    let mut fx = b2bua::effects::HandlerEffects::new();
    let discharged = exec.discharge(&mut call, &mut fx, &event, source_leg_id);
    (discharged, call, fx)
}

/// The caller's ACK discharges the retained 2xx alongside its §13.3.1.4
/// ladder — both timers leave the ledger and the driver, and the bytes can
/// never be sent again, so they stop riding every replication payload for
/// the rest of the call. The rules then see the discharge as a fact.
#[test]
fn caller_ack_discharges_the_retained_answered_2xx() {
    let call = call_with_retained_answered_2xx();
    let (discharged, after, fx) = discharge_ack(&call, "a");
    assert_eq!(discharged, Some(answered_obligation()), "the a-leg ACK names the retained answer");
    assert!(
        after.a_leg.dialogs.first().expect("dialog kept").ext.answered_2xx.is_none(),
        "the retained datagram is discharged from the replicated body",
    );
    assert!(
        after.timers.iter().all(|t| !matches!(
            t.timer_type,
            TimerType::Rung { .. } | TimerType::RepeatGiveUp { .. }
        )),
        "both ladder timers leave the ledger, got {:?}",
        after.timers,
    );
    let cancelled: Vec<&str> = fx
        .critical
        .iter()
        .filter_map(|e| match e {
            CriticalStateEffect::CancelTimer { id } => Some(id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancelled,
        vec!["Rung:AckOf2xx:a:btag:1", "RepeatGiveUp:AckOf2xx:a:btag:1"],
        "both ladder timers leave the driver under the one id recipe",
    );
}

/// A b-leg ACK guards nothing on the a-leg: the retained 2xx (still awaiting
/// the CALLER's ACK) stays, exactly like the ladder timers it rides with.
#[test]
fn callee_side_ack_leaves_the_retained_answered_2xx() {
    let call = call_with_retained_answered_2xx();
    let (discharged, after, fx) = discharge_ack(&call, "b-1");
    assert_eq!(discharged, None, "a b-leg ACK must not discharge the caller's obligation");
    assert!(
        after.a_leg.dialogs.first().expect("dialog kept").ext.answered_2xx.is_some(),
        "the retained datagram still awaits the caller's ACK",
    );
    assert_eq!(after.timers.len(), call.timers.len(), "the ladder keeps running");
    assert!(fx.critical.is_empty(), "nothing is cancelled, got {:?}", fx.critical);
}

/// `relay-ack` owns none of the ladder plumbing any more: an a-leg ACK
/// produces the relay alone.
#[test]
fn relay_ack_pushes_no_ladder_plumbing() {
    let call = call_with_retained_answered_2xx();
    let ack = in_dialog_request(Method::Ack);
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(ack)),
        src: "127.0.0.1:5060".parse().unwrap(),
        matched_client_txn: false,
    };
    let config = B2buaConfig::default();
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
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    let relay_ack = ranked.iter().find(|r| r.id == "relay-ack").expect("relay-ack is a candidate");
    let actions = (relay_ack.handle)(&ctx).expect("relay-ack handles an ACK").actions;
    assert!(
        actions.iter().all(|a| matches!(a, RuleAction::RelayToPeer { .. })),
        "relay-ack relays and nothing else, got {actions:?}",
    );
}

// ADR-0029 X5 — a ladder give-up's settlement after the rules (`settle_give_up`,
// the router's step after the rule chain on every `RepeatGiveUp`): an un-ACKed
// 2xx ends the session whatever a service rule made of the give-up, and a
// reliable provisional's give-up is left to the rules' own RFC 3262 §3 policy.
mod ladder_give_up {
    use super::*;
    use b2bua::effects::OutboundBody;
    use call::{CdrEventType, Obligation, RetainedEmission, Unacked2xx};
    use sip_retransmit::Class;

    const A_TAG: &str = "a-svc";

    /// An answered, bridged call: the a-leg confirmed under `A_TAG` with its
    /// INITIAL 2xx (CSeq 1) still awaiting the caller's ACK, the b-leg
    /// confirmed and ACKed.
    fn answered_call() -> call::Call {
        let mut call = test_call();
        call.a_leg.state = LegState::Confirmed;
        call.a_leg.dialogs = vec![Dialog {
            sip: StackDialog {
                call_id: call.a_leg.call_id.clone(),
                local_tag: A_TAG.into(),
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
                pending_reinvite_2xx: None,
                answered_2xx: Some(retained_2xx(A_TAG, 1, 5060)),
                emitted_ack: None,
                awaited_ack_cseq: None,
            },
        }];
        let mut b = super::b_leg_pending();
        b.state = LegState::Confirmed;
        b.disposition = LegDisposition::Bridged;
        b.dialogs[0].sip.remote_tag = "bobtag".into();
        b.dialogs[0].ext.remote_cseq = Some(1);
        call::helpers::add_b_leg(call, b)
    }

    fn retained_2xx(dialog_tag: &str, cseq: i64, port: u16) -> Unacked2xx {
        Unacked2xx {
            dialog_tag: dialog_tag.into(),
            cseq,
            emission: RetainedEmission::paced(
                b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
                ("127.0.0.1".into(), port),
                Class::Final2xx,
                call::Repeated::response("INVITE", 200),
            )
            .0,
        }
    }

    fn give_up_of(call: &call::Call, obligation: &Obligation) -> CallEvent {
        CallEvent::Timer {
            timer_type: TimerType::RepeatGiveUp { obligation: obligation.clone() },
            call_ref: call.call_ref.clone(),
            leg_id: None,
        }
    }

    fn is_2xx_give_up(ctx: &RuleContext) -> bool {
        matches!(
            ctx.timer_type(),
            Some(TimerType::RepeatGiveUp { obligation: Obligation::AckOf2xx { .. } })
        )
    }

    fn is_prack_give_up(ctx: &RuleContext) -> bool {
        matches!(
            ctx.timer_type(),
            Some(TimerType::RepeatGiveUp { obligation: Obligation::PrackOf { .. } })
        )
    }

    /// A service rule that answers the give-up and deliberately declines to
    /// end anything — it "parks" the call. Ranked above every CORE rule.
    fn parking_rule(filter: fn(&RuleContext) -> bool) -> RuleDefinition {
        RuleDefinition::core(
            "svc-parks-the-give-up",
            SERVICE_LAYER,
            &[],
            Match::timer().call_state(CallModelState::Active).filter(filter),
            |_| Some(RuleHandleResult::new(vec![])),
        )
    }

    /// The router's give-up turn, step by step: scrub the ladder, run the
    /// rules, settle the verdict.
    fn give_up_turn(
        call: &call::Call,
        obligation: &Obligation,
        rules: &[RuleDefinition],
    ) -> (HandlerResult, HandlerResult) {
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let event = give_up_of(call, obligation);
        let mut call = call.clone();
        let mut fx = b2bua::effects::HandlerEffects::new();
        exec.give_up(&mut call, &mut fx, obligation);
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
        let picked: Vec<&str> = pick_ranked(rules, &call, &ctx).iter().map(|r| r.id).collect();
        assert_eq!(
            picked.first().copied(),
            Some("svc-parks-the-give-up"),
            "the service rule outranks CORE: {picked:?}"
        );
        let ruled =
            execute_rules(rules, &call, &ctx, &exec, &b2bua::obligations::ObligationSet::core());
        let settled = exec.settle_give_up(ruled.clone(), obligation, &ctx);
        (ruled, settled)
    }

    fn byes_in(result: &HandlerResult) -> Vec<&str> {
        result
            .effects
            .outbound
            .iter()
            .filter(|e| matches!(&e.body, OutboundBody::Request(r) if *r.method() == Method::Bye))
            .filter_map(|e| e.leg_id.as_deref())
            .collect()
    }

    #[test]
    fn a_service_that_parks_the_unacked_2xx_give_up_does_not_keep_the_session() {
        let call = answered_call();
        let obligation = Obligation::AckOf2xx {
            leg: call.a_leg.leg_id.clone(),
            dialog_tag: A_TAG.into(),
            cseq: 1,
        };
        let mut rules = vec![parking_rule(is_2xx_give_up)];
        rules.extend(default_rules());

        let (ruled, settled) = give_up_turn(&call, &obligation, &rules);

        // What the rules alone left: the defect's shape.
        assert_eq!(ruled.call.state, CallModelState::Active, "the service parked the call");
        assert!(
            ruled.call.a_leg.dialogs[0].ext.answered_2xx.is_some(),
            "the rules alone keep the retained 2xx"
        );

        // The settlement: the session ends with the CORE verdict, and nothing
        // rides the replicated body any more.
        assert_eq!(
            settled.call.state,
            CallModelState::Terminating,
            "RFC 3261 §13.3.1.4: the session ends"
        );
        assert!(
            settled.call.a_leg.dialogs[0].ext.answered_2xx.is_none(),
            "no retained datagram survives the give-up"
        );
        let mut byes = byes_in(&settled);
        byes.sort_unstable();
        assert_eq!(
            byes,
            vec!["a", "b-1"],
            "the a-leg dialog and the b-leg it bridges are both BYEd"
        );
        let marker = settled
            .call
            .cdr_events
            .iter()
            .find(|e| e.event_type == CdrEventType::Bye && e.leg_id == "a")
            .and_then(|e| e.reason.clone());
        assert_eq!(marker.as_deref(), Some("ack_timeout"), "the CDR marker is the CORE rule's");
    }

    #[test]
    fn a_parked_reinvite_2xx_give_up_ends_the_session_under_its_own_marker() {
        let mut call = answered_call();
        // The caller's initial ACK arrived; the b-leg's re-INVITE 2xx (CSeq 7)
        // is the one still owed.
        call.a_leg.dialogs[0].ext.answered_2xx = None;
        call.b_legs[0].dialogs[0].ext.pending_reinvite_2xx = Some(retained_2xx("svc", 7, 5070));
        let obligation =
            Obligation::AckOf2xx { leg: "b-1".into(), dialog_tag: "svc".into(), cseq: 7 };
        let mut rules = vec![parking_rule(is_2xx_give_up)];
        rules.extend(default_rules());

        let (_, settled) = give_up_turn(&call, &obligation, &rules);

        assert_eq!(settled.call.state, CallModelState::Terminating);
        assert!(
            settled.call.b_legs[0].dialogs[0].ext.pending_reinvite_2xx.is_none(),
            "the re-INVITE 2xx is not retained either"
        );
        let marker = settled
            .call
            .cdr_events
            .iter()
            .find(|e| e.event_type == CdrEventType::Bye && e.leg_id == "b-1")
            .and_then(|e| e.reason.clone());
        assert_eq!(
            marker.as_deref(),
            Some("reinvite_ack_timeout"),
            "the CDR names the leg that owed the ACK"
        );
    }

    #[test]
    fn a_prack_give_up_keeps_the_rules_own_verdict() {
        // An answered call whose caller-facing reliable provisional was never
        // PRACKed: RFC 3262 §3's reject has nothing left to answer, and CORE
        // absorbs. The settlement forces nothing — the §13.3.1.4 floor is the
        // 2xx's alone.
        let mut call = answered_call();
        call.a_leg.dialogs[0].ext.answered_2xx = None;
        call.reliable_provisionals.push(call::ReliableProvisional {
            a_tag: A_TAG.into(),
            a_rseq: 1,
            a_cseq: Some(1),
            b_leg_id: "b-1".into(),
            b_tag: "bobtag".into(),
            b_cseq: 1,
            b_rseq: 9,
            acknowledged: false,
            emission: Some(
                RetainedEmission::paced(
                    b"SIP/2.0 183 Session Progress\r\n\r\n".to_vec(),
                    ("127.0.0.1".into(), 5060),
                    Class::ReliableProvisional,
                    call::Repeated::response("INVITE", 183),
                )
                .0,
            ),
        });
        let obligation = Obligation::PrackOf { a_tag: A_TAG.into(), a_rseq: 1 };
        let mut rules = vec![parking_rule(is_prack_give_up)];
        rules.extend(default_rules());

        let (ruled, settled) = give_up_turn(&call, &obligation, &rules);

        assert_eq!(ruled.call.state, CallModelState::Active);
        assert_eq!(
            settled.call.state,
            CallModelState::Active,
            "a PRACK give-up is the rules' to answer"
        );
        assert!(byes_in(&settled).is_empty(), "no forced teardown");
        assert!(
            settled.call.reliable_provisionals[0].emission.is_none(),
            "the provisional's emission is spent with its ladder"
        );
    }
}

// ── The going-away gate (executor) ───────────────────────────────────────────
//
// An asynchronous trigger — a timer fire, a transaction timeout, an
// internal-event fold — on a `Terminating`/`Terminated` call reaches only its
// teardown rules (`RuleDefinition::teardown`); every other candidate is
// absorbed and counted. A peer's message keeps its per-rule filters, and
// `BeginTermination` disarms every service watchdog and deactivates every
// service machine, so the fire is stopped at the source too.
mod going_away_gate {
    use super::*;
    use b2bua::obligations::ObligationSet;
    use call::CdrEventType;

    const SVC: MachineId = MachineId::new("svc-gate");
    const DEADLINE: TimerType = TimerType::service(SVC, "deadline");
    static ARMED: [StateLabel; 1] = [StateLabel::new("Armed")];
    static TO_TERMINAL: [(StateLabel, StateLabel); 1] =
        [(StateLabel::new("Armed"), StateLabel::terminal())];
    static REJECT_EFFECTS: [Effect; 2] = [
        Effect::Respond { status: 480, label: "answer the caller" },
        Effect::LifecycleCommand { label: "tear the call down" },
    ];
    const SCRUB_REASON: &str = "deadline-scrub";

    /// The misbehaving shape: a deadline that answers the caller and tears
    /// down, guard-less.
    fn reject_on_deadline(_: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![
            RuleAction::RespondToALeg {
                status: 480,
                reason: "Temporarily Unavailable".into(),
                header_updates: vec![],
                contacts: vec![],
            },
            RuleAction::BeginTermination { reason: Some("deadline".into()) },
            RuleAction::ClearState { machine: SVC },
        ]))
    }

    /// A teardown rule's shape: the same deadline only books its passing.
    fn scrub_on_deadline(_: &RuleContext) -> Option<RuleHandleResult> {
        Some(RuleHandleResult::new(vec![RuleAction::AddCdrEvent {
            event_type: CdrEventType::Timeout,
            leg_id: "a".into(),
            status_code: None,
            reason: Some(SCRUB_REASON.into()),
        }]))
    }

    fn watchdog_rule(
        id: &'static str,
        handle: fn(&RuleContext) -> Option<RuleHandleResult>,
    ) -> RuleDefinition {
        RuleDefinition {
            id,
            layer: SERVICE_LAYER,
            overrides: &[],
            matcher: Match::timer().timer_type(DEADLINE),
            handle,
            machine: Some(SVC),
            active_states: &ARMED,
            transitions: &TO_TERMINAL,
            effects: &REJECT_EFFECTS,
            teardown: false,
        }
    }

    /// A ringing call with the service seeded `Armed` in `state`.
    fn armed_call(state: CallModelState) -> call::Call {
        let mut call = test_call();
        call.state = state;
        call.a_leg.state = LegState::Early;
        call = call::helpers::add_b_leg(call, b_leg_pending());
        call.sm_cursors.insert(SVC, StateLabel::new("Armed"));
        call
    }

    fn deadline_fire(call: &call::Call) -> CallEvent {
        CallEvent::Timer { timer_type: DEADLINE, call_ref: call.call_ref.clone(), leg_id: None }
    }

    fn run(rules: &[RuleDefinition], call: &call::Call, event: &CallEvent) -> HandlerResult {
        let config = B2buaConfig::default();
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let ctx = ctx_for(call, event, &config);
        execute_rules(rules, call, &ctx, &exec, &ObligationSet::core())
    }

    fn absorbed_of(result: &HandlerResult) -> Vec<(&'static str, &'static str)> {
        result
            .effects
            .buffered
            .iter()
            .filter_map(|e| match e {
                BufferedObservabilityEffect::GoingAwayAbsorbed { event, rule } => {
                    Some((*event, *rule))
                }
                _ => None,
            })
            .collect()
    }

    #[test]
    fn a_service_watchdog_on_a_going_away_call_is_absorbed_and_counted() {
        for state in [CallModelState::Terminating, CallModelState::Terminated] {
            let call = armed_call(state);
            let rules = vec![watchdog_rule("gate-reject", reject_on_deadline)];
            let result = run(&rules, &call, &deadline_fire(&call));
            assert!(result.effects.outbound.is_empty(), "nothing leaves on {state:?}");
            assert_eq!(result.call.state, state, "the rule never ran: no lifecycle move");
            assert_eq!(
                result.call.sm_cursors.get(&SVC),
                Some(&StateLabel::new("Armed")),
                "the rule never ran: its cursor did not move",
            );
            assert_eq!(
                absorbed_of(&result),
                vec![("timer", "gate-reject")],
                "the absorbed fire is counted once, naming the rule",
            );
        }
    }

    #[test]
    fn a_teardown_rule_still_runs_on_a_going_away_call() {
        let call = armed_call(CallModelState::Terminating);
        let rules = vec![
            watchdog_rule("gate-reject", reject_on_deadline),
            watchdog_rule("gate-scrub", scrub_on_deadline).runs_while_terminating(),
        ];
        let result = run(&rules, &call, &deadline_fire(&call));
        assert!(result.effects.outbound.is_empty(), "the guard-less rule was absorbed");
        assert!(
            result.call.cdr_events.iter().any(|e| e.reason.as_deref() == Some(SCRUB_REASON)),
            "the teardown rule ran: {:?}",
            result.call.cdr_events,
        );
        assert_eq!(absorbed_of(&result), vec![("timer", "gate-reject")]);
    }

    #[test]
    fn the_gate_is_inert_on_a_live_call() {
        let call = armed_call(CallModelState::Active);
        let rules = vec![watchdog_rule("gate-reject", reject_on_deadline)];
        let result = run(&rules, &call, &deadline_fire(&call));
        assert!(
            result.effects.outbound.iter().any(|e| e.label.starts_with("480")),
            "on a live call the deadline answers the caller: {:?}",
            result.effects.outbound.iter().map(|e| &e.label).collect::<Vec<_>>(),
        );
        assert!(absorbed_of(&result).is_empty(), "nothing absorbed on a live call");
    }

    #[test]
    fn a_peer_message_is_not_gated() {
        // A machine-bound rule on an in-dialog INFO stays a candidate on a
        // terminating call: the gate covers the call's own clocks, not what a
        // peer says.
        let mut call = test_call();
        call.state = CallModelState::Terminating;
        call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
        let event = info_event();
        let config = B2buaConfig::default();
        let ctx = ctx_for(&call, &event, &config);
        let rules = vec![sm_rule(handle_to_s1)];
        assert_eq!(pick_ranked(&rules, &call, &ctx).len(), 1, "a peer's INFO reaches the rule");
    }

    #[test]
    fn begin_termination_disarms_service_watchdogs_and_deactivates_machines() {
        // Entering `terminating` takes every service watchdog out of the
        // ledger and the driver — beside the per-leg NoAnswer entries — and
        // removes every service machine's cursor; the call-level deadline and
        // the lifecycle projection stay.
        let mut call = armed_call(CallModelState::Active);
        let no_answer_id = TimerType::NoAnswer.timer_id(Some("b-1"));
        let deadline_id = DEADLINE.timer_id(None);
        for (id, timer_type, leg_id) in [
            (no_answer_id.clone(), TimerType::NoAnswer, Some("b-1".to_string())),
            (deadline_id.clone(), DEADLINE, None),
            (TimerType::GlobalDuration.timer_id(None), TimerType::GlobalDuration, None),
        ] {
            call.timers.push(call::TimerEntry { id, timer_type, fire_at: 5_000, leg_id });
        }
        call.sm_cursors
            .insert(b2bua::rules::invariants::GLOBAL_CALL_MACHINE, StateLabel::new("Active"));
        let event = CallEvent::Cancelled {
            call_id: call.a_leg.call_id.clone(),
            from_tag: call.a_leg.from_tag.clone(),
            invite_cseq: None,
            in_dialog: false,
            headers: vec![],
        };
        let config = B2buaConfig::default();
        let ctx = ctx_for(&call, &event, &config);
        let id_gen = IdGen::seeded(1);
        let exec = ActionExecutor {
            config: &config,
            id_gen: &id_gen,
            now_ms: 0,
            wire_faults: &b2bua::wire_faults::WireFaults::none(),
        };
        let result = exec.execute(
            &[RuleAction::BeginTermination { reason: Some("CANCEL".into()) }],
            &call,
            &ctx,
        );
        let ledger: Vec<&str> = result.call.timers.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            ledger,
            vec!["GlobalDuration", "TerminatingTimeout"],
            "NoAnswer and the service watchdog left the ledger; the call-level cap stays",
        );
        for id in [&no_answer_id, &deadline_id] {
            assert!(
                result.effects.critical.iter().any(
                    |e| matches!(e, CriticalStateEffect::CancelTimer { id: cancelled } if cancelled == id)
                ),
                "the live fiber {id} is cancelled, got {:?}",
                result.effects.critical,
            );
        }
        let machines: Vec<&str> = result.call.sm_cursors.keys().map(|m| m.as_str()).collect();
        assert_eq!(machines, vec!["global-call"], "every service machine is deactivated");
    }
}
