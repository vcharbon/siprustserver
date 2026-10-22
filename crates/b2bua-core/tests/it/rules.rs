//! Rule-engine unit tests: matcher ranking + overrides, invariant enforcement,
//! and a representative default-rule's action output — pinned at the rule seam
//! without a full SUT.

use std::net::SocketAddr;

use b2bua_core::config::B2buaConfig;
use b2bua_core::effects::{BufferedObservabilityEffect, CriticalStateEffect, HandlerResult};
use b2bua_core::initial_invite::build_initial_call;
use b2bua_core::obligations::ObligationSet;
use b2bua_core::rules::{
    default_rules, execute_rules, invariants, pick_ranked, ActionExecutor, Effect, Match,
    RuleAction, RuleCall, RuleContext, RuleDefinition, RuleHandleResult, TimerDelay, SERVICE_LAYER,
};
use b2bua_core::turn_notes::TurnNotes;
use b2bua_core::turn_scope::TurnScope;
use b2bua_sdk::event::CallEvent;
use b2bua_sdk::model::{Body, BodyAuthor, LegEnd};
use call::{
    CallModelState, Dialog, DialogState, Direction, EndingBy, Leg, LegKind, LegState, MachineId,
    OutboundInvite, PeerDialog, RemoteInfo, StateLabel, TerminationCause, TimerKey, TimerType,
};
use sip_ids::TurnIds;
use sip_message::generators::{
    generate_out_of_dialog_request, CapabilitySet, GenerateOutOfDialogRequestOpts,
    OutOfDialogMethod,
};
use sip_message::header::{self, Uri, Via};
use sip_message::parser::custom::CustomParser;
use sip_message::{HeaderName, Method, SipMessage, SipParser, SipRequest, SipStr};

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
    generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts).expect("an INVITE")
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

/// A turn under the fixed test key, numbered `turn_seq`.
fn scope(turn_seq: u64) -> TurnScope {
    TurnScope::of(TurnIds::fixed(turn_seq))
}

fn test_call() -> call::Call {
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    build_initial_call(&invite(), src, &B2buaConfig::default(), &mut scope(1).ids, 0)
}

/// A call whose a-leg already carries the early dialog `a1` (the tag a relayed
/// or minted provisional pinned), the leg itself still Trying.
fn call_with_early_a_dialog(a1: &str) -> call::Call {
    let mut call = test_call();
    call.a_leg.dialogs = vec![a_dialog(&call, a1, DialogState::Early)].into();
    call.a_leg.own_tag = Some(a1.into());
    call
}

/// Confirm the caller's leg: her early dialog, else a dialog of hers under
/// `atag`.
fn confirm_a(call: &mut call::Call) {
    if !call.a_leg.dialogs.iter().any(|d| d.state == DialogState::Early) {
        let d = a_dialog(call, "atag", DialogState::Confirmed);
        call.a_leg.dialogs.insert(d);
        call.a_leg.own_tag.get_or_insert_with(|| "atag".into());
    }
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
}

/// A dialog of the caller's leg under `tag`, in `state`, both sequences at 1.
fn a_dialog(call: &call::Call, tag: &str, state: DialogState) -> Dialog {
    let mut d = Dialog::new(tag, call.a_leg_invite.from_tag.clone(), state, 1);
    d.peer_cseq = Some(1);
    d.remote_target = "sip:alice@127.0.0.1:5060".into();
    d
}

/// Give `leg` — dialled under `svc` — the dialog the peer tag `peer_tag`
/// names, confirmed on a confirmed leg, early otherwise, in place of a dialog
/// [`call::testkit::stage_leg`] minted; return it.
fn with_dialog<'a>(leg: &'a mut Leg, peer_tag: &str) -> &'a mut Dialog {
    let state = match leg.state() {
        LegState::Confirmed => DialogState::Confirmed,
        _ => DialogState::Early,
    };
    leg.dialogs.retain(|d| d.peer_tag != "staged-peer");
    let mut d = Dialog::new("svc", peer_tag, state, 1);
    d.remote_target = "sip:bob@10.0.0.2:5070".into();
    leg.dialogs.insert(d);
    leg.dialogs.by_tags_mut("svc", peer_tag).expect("the dialog just inserted")
}

#[test]
fn timer_global_duration_selects_max_duration() {
    let call = test_call();
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("max-duration"));
}

/// A rule reads the decision ordinal current at its turn: `0` on a call no
/// decision touched, then the count of applied decisions.
#[test]
fn rule_call_reads_the_current_decision_ordinal() {
    let call = test_call();
    assert_eq!(RuleCall::new(&call).decision_ordinal(), 0, "no decision yet");
    let call = call::helpers::mark_decision(
        call,
        1_000,
        call::DecisionKind::Route,
        Some("a".to_string()),
        Some("first".to_string()),
    );
    assert_eq!(RuleCall::new(&call).decision_ordinal(), 1, "one decision applied");
}

/// The `no-answer` rule's output for a fire naming `fired_leg_id` on `call`.
fn no_answer_result(call: &call::Call, fired_leg_id: &str) -> Vec<RuleAction> {
    let event = CallEvent::Timer {
        key: TimerKey::leg(TimerType::NoAnswer, fired_leg_id),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let no_answer = ranked.iter().find(|r| r.id == "no-answer").expect("no-answer is a candidate");
    (no_answer.handle)(&ctx).expect("no-answer handles its own timer").actions
}

#[test]
fn no_answer_fire_on_an_answered_caller_is_absorbed() {
    // The caller's INVITE took its 2xx: no final may be authored on it. The
    // fire is absorbed with no action — the fired instance already left the
    // ledger at its delivery.
    let mut call = test_call();
    confirm_a(&mut call);
    let mut b = b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, b);
    let actions = no_answer_result(&call, "b-1");
    assert!(actions.is_empty(), "absorbed, got {actions:?}");
}

#[test]
fn no_answer_fire_naming_an_absent_leg_on_an_answered_caller_is_absorbed() {
    // The caller is answered: whatever leg the fire names, no final may be
    // authored on her INVITE.
    let mut call = test_call();
    confirm_a(&mut call);
    let mut b = b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, b);
    let actions = no_answer_result(&call, "b-ghost");
    assert!(actions.is_empty(), "absorbed, got {actions:?}");
}

#[test]
fn no_answer_fire_on_own_pending_leg_fires_despite_other_leg_confirmed() {
    // The guard is per-leg, not call-level: another leg being Confirmed does
    // not spend a fire whose OWN leg is still awaiting an answer — the normal
    // body (CDR timeout + DestroyLeg on the fired leg) runs.
    let mut call = test_call();
    let mut confirmed = b_leg_pending();
    call::testkit::stage_leg(&mut confirmed, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, confirmed);
    let mut pending = b_leg_pending();
    pending.leg_id = "b-2".into();
    call::testkit::stage_leg(&mut pending, LegState::Trying);
    call = call::helpers::add_b_leg(call, pending);
    let actions = no_answer_result(&call, "b-2");
    // No consult here: the caller's 480 and the teardown follow the destroy.
    match actions.as_slice() {
        [RuleAction::AddCdrEvent { leg_id, reason, .. }, RuleAction::DestroyLeg { leg_id: destroyed, .. }, RuleAction::RespondToALeg { status: 480, .. }, RuleAction::BeginTermination { .. }]
            if leg_id == "b-2"
                && reason.as_deref() == Some("no_answer_timeout")
                && destroyed == "b-2" => {}
        other => panic!("genuine fire runs the normal no-answer body, got {other:?}"),
    }
}

/// The executor applying `actions` to `call` on a caller-side event.
fn executed(call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    exec.execute(actions, call, &ctx)
}

#[test]
fn cancelling_a_leg_retires_its_timers() {
    // A CANCELled leg awaits no answer: its ring deadline and every other
    // timer it guards leave the ledger and the driver with the CANCEL, and
    // the call's own timers stay.
    let mut call = test_call();
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let no_answer = TimerKey::leg(TimerType::NoAnswer, "b-1");
    let duration = TimerKey::call(TimerType::GlobalDuration);
    call::helpers::arm_timer(&mut call, no_answer.clone(), 5_000);
    call::helpers::arm_timer(&mut call, duration.clone(), 3_600_000);
    let result = executed(&call, &[RuleAction::CancelLeg { leg_id: "b-1".into() }]);
    let keys: Vec<_> = result.call.timers.iter().map(|t| t.key.clone()).collect();
    assert_eq!(keys, vec![duration], "only the call's timer stays");
    assert!(
        result
            .effects
            .critical
            .iter()
            .any(|e| matches!(e, CriticalStateEffect::CancelTimer { key } if *key == no_answer)),
        "the live NoAnswer fiber is cancelled, got {:?}",
        result.effects.critical,
    );
}

/// Record this stack's CANCEL of `leg`'s INVITE: the leg reads `Cancelling`.
fn cancel_sent(leg: &mut Leg) {
    leg.outbound_invite.as_mut().expect("a dialled leg").cancel_sent = true;
}

/// Put every dialog of `leg` in `state`.
fn end_dialogs(leg: &mut Leg, state: DialogState) {
    for d in leg.dialogs.iter_mut() {
        d.state = state.clone();
    }
}

#[test]
fn setup_timeout_fire_on_a_terminating_call_is_absorbed() {
    // A terminating call authors no new final on the a-leg's completed
    // transaction: the fire is absorbed, no 408.
    let mut call = test_call();
    call.state = CallModelState::Terminating;
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::SetupTimeout),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    let setup_timeout =
        ranked.iter().find(|r| r.id == "setup-timeout").expect("setup-timeout is a candidate");
    let actions = (setup_timeout.handle)(&ctx).expect("setup-timeout handles its timer").actions;
    assert!(actions.is_empty(), "absorbed: no 408, got {actions:?}");
}

/// The `max-duration` rule's output for a `GlobalDuration` fire on `call`.
fn max_duration_result(call: &call::Call) -> Vec<RuleAction> {
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::GlobalDuration),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let max_duration =
        ranked.iter().find(|r| r.id == "max-duration").expect("max-duration is a candidate");
    (max_duration.handle)(&ctx).expect("max-duration handles its timer").actions
}

#[test]
fn max_duration_fire_on_a_terminating_call_is_absorbed() {
    // The cap crossing the terminating window (caller BYE'd just before it, or
    // a reclaim restored a stale entry): even the consult-eligible shape —
    // answered, subscribed, callback_context — is spent on a going-away call.
    // No `call_release` consult (a `reroute` outcome would dial a fresh b-leg),
    // no BeginTermination re-arm — absorbed.
    let mut call = test_call();
    confirm_a(&mut call);
    call.state = CallModelState::Terminating;
    call.callback_context = Some("cb".into());
    call.subscriptions = vec![call::ReleaseEventKind::MaxCallDuration];
    let actions = max_duration_result(&call);
    assert!(actions.is_empty(), "absorbed, got {actions:?}");
}

#[test]
fn max_duration_fire_on_a_live_subscribed_call_still_consults() {
    // The going-away guard is narrow: an Active answered subscribed call's cap
    // keeps the `call_release` consult.
    let mut call = test_call();
    confirm_a(&mut call);
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
        incarnation: None,
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
    // A `/call/new` decision result landing on a call already going away drives
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
    cancel_sent(&mut b);
    call = call::helpers::add_b_leg(call, b);
    let no_answer = TimerKey::leg(TimerType::NoAnswer, "b-1");
    call::helpers::arm_timer(&mut call, no_answer.clone(), 5_000);
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let result = exec.execute(
        &[RuleAction::BeginTermination {
            reason: Some("CANCEL".into()),
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
        }],
        &call,
        &ctx,
    );
    assert!(
        !result.call.timers.iter().any(|t| *t.timer_type() == TimerType::NoAnswer),
        "no NoAnswer entry survives BeginTermination: {:?}",
        result.call.timers,
    );
    assert!(
        result
            .effects
            .critical
            .iter()
            .any(|e| matches!(e, CriticalStateEffect::CancelTimer { key } if *key == no_answer)),
        "the live NoAnswer fiber is cancelled, got {:?}",
        result.effects.critical,
    );
}

/// A callee's 2xx relayed toward the caller lost the race to her CANCEL: the
/// transaction layer had already answered her INVITE 487 (RFC 3261 §9.2), so
/// the 2xx never left and she holds no dialog. The `Cancelled` turn records
/// the 487 over the record's 2xx, and the termination sends her no BYE
/// (§15: a BYE names a dialog that exists).
#[test]
fn a_2xx_the_cancel_beat_to_the_caller_leaves_her_no_dialog_to_bye() {
    let mut call = call_with_early_a_dialog("a1");
    confirm_a(&mut call);
    call.a_leg.invite_final_sent = Some(200);
    let call = call::helpers::record_cancelled_invite(call, "a");
    assert_eq!(call.a_leg.invite_final_sent, Some(487), "the wire's final is the layer's 487");
    let result = execute_on(
        &call,
        &[RuleAction::BeginTermination {
            reason: Some("CANCEL".into()),
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
        }],
    );
    let to_a: Vec<String> = result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("a"))
        .map(|e| e.label.clone())
        .collect();
    assert!(to_a.is_empty(), "nothing goes to the caller, got {to_a:?}");
    assert_eq!(result.call.a_leg.state(), LegState::Terminated);
}

#[test]
fn in_dialog_bye_selects_relay_bye() {
    let mut call = test_call();
    // An in-dialog BYE (carries a To-tag) on the established call.
    confirm_a(&mut call);
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("relay-bye"));
}

/// A caller's BYE while its INVITE is still pending (RFC 3261 §15: the early
/// dialog a tagged provisional opened, the local tag on the a-dialog; the
/// a-leg itself reads Trying until its answer) selects `early-dialog-bye`, never
/// `relay-bye`: the BYE is answered 200, the pending INVITE 487 (§15.1.2), and
/// the call terminates as a caller release. An a-leg whose INVITE already
/// carries a final is not this rule's.
#[test]
fn bye_on_the_early_a_dialog_selects_early_dialog_bye_with_487_and_termination() {
    let call = call_with_early_a_dialog("btag");
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("early-dialog-bye"));
    let actions = (ranked[0].handle)(&ctx).expect("the rule handles the BYE").actions;
    let bye_ok = actions.iter().position(|a| matches!(a, RuleAction::Respond { status: 200, .. }));
    let invite_487 =
        actions.iter().position(|a| matches!(a, RuleAction::RespondToALeg { status: 487, .. }));
    assert!(bye_ok < invite_487, "200 to the BYE, then 487 to the INVITE, got {actions:?}");

    let mut answered = call.clone();
    answered.a_leg.invite_final_sent = Some(487);
    let ctx = RuleContext { call: RuleCall::new(&answered), ..ctx };
    let ranked = pick_ranked(&rules, &answered, &ctx);
    assert_ne!(
        ranked.first().map(|r| r.id),
        Some("early-dialog-bye"),
        "an INVITE that carries its final draws no second one"
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            RuleAction::BeginTermination { cause: TerminationCause::RemoteBye, by_leg: Some(l), .. } if l == "a"
        )),
        "a caller release, got {actions:?}",
    );
}

/// The same BYE on a call already terminating (a b-leg's BYE ended it while
/// the caller still rang, so the termination stamped the early a-leg's BYE
/// disposition): still `early-dialog-bye`, never `resolve-ended-leg-bye`,
/// which would answer the BYE alone; the two finals go out, and the
/// teardown under way is not restarted.
#[test]
fn bye_on_the_early_a_dialog_of_a_terminating_call_still_answers_the_invite() {
    let mut call = call_with_early_a_dialog("btag");
    call.state = CallModelState::Terminating;
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("early-dialog-bye"));
    let actions = (ranked[0].handle)(&ctx).expect("the rule handles the BYE").actions;
    assert!(
        actions.iter().any(|a| matches!(a, RuleAction::RespondToALeg { status: 487, .. })),
        "the pending INVITE gets its 487, got {actions:?}",
    );
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
        "a terminating call is not torn down twice, got {actions:?}",
    );
}

/// A BYE from a leg this stack already ended (Terminated, its own BYE sent)
/// on a call that stays Active selects `resolve-ended-leg-bye`, never
/// `relay-bye`: the leg is no longer a party of the session, so its crossing
/// BYE (RFC 3261 §15.1.2) ends that dialog alone and tears nothing down.
#[test]
fn bye_from_an_ended_leg_selects_resolve_ended_leg_bye_without_termination() {
    let mut call = test_call();
    confirm_a(&mut call);
    let mut ended = b_leg_pending();
    call::testkit::stage_leg(&mut ended, LegState::Confirmed);
    end_dialogs(&mut ended, DialogState::Ending(EndingBy::Bye));
    call = call::helpers::add_b_leg(call, ended);
    assert_eq!(call.state, CallModelState::Active, "only one leg was released");

    let bye = in_dialog_request(sip_message::Method::Bye);
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(bye)),
        src: "10.0.0.2:5070".parse().unwrap(),
        matched_client_txn: false,
    };
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert_eq!(ranked.first().map(|r| r.id), Some("resolve-ended-leg-bye"));
    let actions = (ranked[0].handle)(&ctx).expect("the rule handles the BYE").actions;
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
        "an ended leg's BYE tears nothing down, got {actions:?}",
    );
}

/// A 481 to this stack's own BYE on a leg already resolved by the peer's
/// crossing BYE (`ByeReceived`) selects no rule on an Active call: the BYE
/// final denies nothing the BYE has not already ended (RFC 3261 §15.1.2), so
/// `handle-481` declines it and the session stays up.
#[test]
fn bye_481_on_a_bye_received_leg_selects_no_teardown() {
    let mut call = test_call();
    confirm_a(&mut call);
    let mut ended = b_leg_pending();
    call::testkit::stage_leg(&mut ended, LegState::Confirmed);
    end_dialogs(&mut ended, DialogState::Ended(call::DialogEnd::PeerBye));
    call = call::helpers::add_b_leg(call, ended);
    assert_eq!(call.state, CallModelState::Active, "only one leg was released");

    let raw = "SIP/2.0 481 Call/Transaction Does Not Exist\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 2 BYE\r\n\
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
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    assert!(
        !ranked.iter().any(|r| r.id == "handle-481"),
        "a BYE's 481 is never the peer denying the dialog, got {:?}",
        ranked.iter().map(|r| r.id).collect::<Vec<_>>(),
    );
    for rule in &ranked {
        if let Some(outcome) = (rule.handle)(&ctx) {
            assert!(
                !outcome.actions.iter().any(|a| matches!(a, RuleAction::BeginTermination { .. })),
                "{} tears the session down on a BYE's 481: {:?}",
                rule.id,
                outcome.actions,
            );
        }
    }
}

/// A terminal that names no cause is a writer the funnel does not know: a
/// defect noted on the turn, counted, and the call still cleaned up.
#[test]
fn a_terminal_with_no_termination_record_is_a_defect() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    let mut notes = TurnNotes::default();
    let leave = invariants::UnansweredCaller::Leave;
    let core = ObligationSet::core();
    let result =
        invariants::enforce(&core, &before, HandlerResult::new(call), 0, leave, &mut notes);
    let defects: Vec<_> = notes.defects().iter().map(|d| d.name).collect();
    assert_eq!(defects, [b2bua_core::turn_notes::TERMINATION_RECORDED]);
    assert!(result.effects.critical.iter().any(|e| matches!(e, CriticalStateEffect::RemoveCall)));
}

#[test]
fn invariants_append_cleanup_on_terminated() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    // A synthetic terminal states its cause as every live path does.
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        invariants::UnansweredCaller::Answer(&CapabilitySet::silent()),
        &mut TurnNotes::default(),
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
                && matches!(&e.body, b2bua_core::effects::OutboundBody::Response(r) if r.status() == 503)
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

/// The funnel's 503 to a caller a terminating call leaves unanswered is a
/// final minted in the element's own name: it carries the deployment's
/// advertisement for one.
#[test]
fn the_unanswered_callers_503_carries_the_deployment_advertisement() {
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    let policy = CapabilitySet::stating(
        None,
        None,
        Some(vec![sip_message::header::AcceptRange::new("application/sdp")]),
    );
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        invariants::UnansweredCaller::Answer(&policy),
        &mut TurnNotes::default(),
    );
    let accept: Vec<String> = result
        .effects
        .outbound
        .iter()
        .find_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Response(r) if r.status() == 503 => {
                Some(r.raw(HeaderName::Accept).map(str::to_string).collect())
            }
            _ => None,
        })
        .expect("the synthesized 503");
    assert_eq!(accept, ["application/sdp"]);
}

#[test]
fn no_synthesized_final_when_the_turn_already_answered() {
    // The reject path: same terminated transition, but THIS turn carries a
    // final to the a-leg (as `reject_call` / `RespondToALeg` do) — the funnel
    // must not double-answer.
    let mut call = test_call();
    let before = call.clone();
    call.state = CallModelState::Terminated;
    // A synthetic terminal states its cause as every live path does.
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    let a_invite = crate::caller_of(&call);
    let mut result = HandlerResult::new(call);
    let effect = b2bua_core::rules::relay::response_to_a_leg(
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
        b2bua_core::effects::Provenance::Authored,
        b2bua_core::rules::relay::Author::Stack,
    )
    .expect("the transaction's first final is admitted");
    result.effects.outbound.push(effect);
    assert_eq!(result.call.a_leg.invite_final_sent, Some(486), "the final is recorded on the leg");
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        result,
        0,
        invariants::UnansweredCaller::Answer(&CapabilitySet::silent()),
        &mut TurnNotes::default(),
    );
    let finals_to_a = result
        .effects
        .outbound
        .iter()
        .filter(|e| {
            e.leg_id.as_deref() == Some("a")
                && matches!(&e.body, b2bua_core::effects::OutboundBody::Response(r) if r.status() >= 200)
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
    // A synthetic terminal states its cause as every live path does.
    call = call::helpers::record_termination(call, 0, TerminationCause::Supervisor, None);
    call = call::helpers::record_invite_final(call, "a", 487);
    let result = invariants::enforce(
        &ObligationSet::core(),
        &before,
        HandlerResult::new(call),
        0,
        invariants::UnansweredCaller::Answer(&CapabilitySet::silent()),
        &mut TurnNotes::default(),
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
    execute_with(&B2buaConfig::default(), call, actions)
}

/// [`execute_on`] under `config`.
fn execute_with(config: &B2buaConfig, call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
    };
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "a",
        direction: Direction::FromA,
        now_ms: 0,
        config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config, turn: &mut id_gen, now_ms: 0 };
    exec.execute(actions, call, &ctx)
}

fn finals_to_a(result: &HandlerResult) -> Vec<u16> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("a"))
        .filter_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Response(r) if r.status() >= 200 => Some(r.status()),
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
            body: None,
            to_tag: None,
            p_early_media: None,
        }],
    );
    let provisionals = result
        .effects
        .outbound
        .iter()
        .filter(
            |e| matches!(&e.body, b2bua_core::effects::OutboundBody::Response(r) if r.status() == 183),
        )
        .count();
    assert_eq!(provisionals, 1, "the 183 leaves: {:?}", result.effects.outbound);
    assert_eq!(result.call.a_leg.invite_final_sent, None, "a provisional records no final");
    assert!(refusals(&result).is_empty());
    assert!(late_provisional_refusals(&result).is_empty());
}

/// The `(status, carried)` pairs of the provisionals `result` refused after
/// the a-leg's final.
fn late_provisional_refusals(result: &HandlerResult) -> Vec<(u16, u16)> {
    result
        .effects
        .buffered
        .iter()
        .filter_map(|e| match e {
            BufferedObservabilityEffect::ProvisionalAfterFinalRefused { status, carried } => {
                Some((*status, *carried))
            }
            _ => None,
        })
        .collect()
}

/// The a-facing provisionals `result` emitted.
fn provisionals_to_a(result: &HandlerResult) -> Vec<u16> {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some("a"))
        .filter_map(|e| match &e.body {
            b2bua_core::effects::OutboundBody::Response(r) if r.status() < 200 => Some(r.status()),
            _ => None,
        })
        .collect()
}

#[test]
fn a_provisional_after_the_final_is_refused() {
    // The a-leg INVITE transaction sent its 200: a provisional a rule authors
    // toward it afterwards is refused at the seam — never built, never on the
    // wire (RFC 3261 §13.3.1.1 / §17.2.1) — and the answered dialog keeps its
    // tag.
    let call = call::helpers::record_invite_final(test_call(), "a", 200);
    let result = execute_on(
        &call,
        &[RuleAction::SendProvisionalToLeg {
            leg_id: "a".into(),
            status: 183,
            reason: "Session Progress".into(),
            body: None,
            to_tag: Some("late-tag".into()),
            p_early_media: None,
        }],
    );
    assert!(
        provisionals_to_a(&result).is_empty(),
        "nothing provisional leaves after the final: {:?}",
        result.effects.outbound
    );
    assert!(
        result.call.a_leg.dialogs.iter().all(|d| d.our_tag != "late-tag"),
        "a refused provisional re-tags no dialog",
    );
    assert_eq!(result.call.a_leg.invite_final_sent, Some(200), "the 200 stands");
    assert_eq!(
        late_provisional_refusals(&result),
        vec![(183, 200)],
        "the refusal is reported against the final the transaction carries"
    );
    assert!(refusals(&result).is_empty(), "a provisional is not a second final");
}

/// The callee's Contact on its provisional.
const CALLEE_RINGING_CONTACT: &str = "sip:bob@10.0.0.2:5070;transport=udp";
/// The proxy that recorded itself on the provisional (RFC 3261 §12.1.2).
const CALLEE_RINGING_ROUTE: &str = "sip:10.0.0.7:5060;lr";

/// A `180 Ringing` to the b-leg's initial INVITE, from callee tag `bobtag`,
/// with the callee's Contact and a recorded route; `reliable` states
/// `Require: 100rel` + `RSeq: 1`.
fn b_leg_ringing_event(reliable: bool) -> CallEvent {
    let reliability = if reliable { "Require: 100rel\r\nRSeq: 1\r\n" } else { "" };
    let raw = format!(
        "SIP/2.0 180 Ringing\r\n\
Via: SIP/2.0/UDP 10.0.0.9:5060;branch=z9hG4bKb\r\n\
Record-Route: <{CALLEE_RINGING_ROUTE}>\r\n\
From: <sip:svc@10.0.0.9:5060>;tag=svc\r\n\
To: <sip:bob@10.0.0.2:5070>;tag=bobtag\r\n\
Call-ID: bcid@x\r\n\
CSeq: 1 INVITE\r\n\
Contact: <{CALLEE_RINGING_CONTACT}>\r\n\
{reliability}Content-Length: 0\r\n\r\n"
    );
    let resp = match CustomParser::new().parse(raw.as_bytes()).unwrap() {
        SipMessage::Response(r) => r,
        _ => panic!("expected a response"),
    };
    CallEvent::Sip {
        message: Box::new(SipMessage::Response(resp)),
        src: "10.0.0.2:5070".parse().unwrap(),
        matched_client_txn: true,
    }
}

/// The actions the first ranked default rule produces for `event` from `leg`.
fn handle_from(call: &call::Call, event: &CallEvent, leg: &str) -> (&'static str, Vec<RuleAction>) {
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event,
        source_leg_id: leg,
        direction: Direction::FromB,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let rule = ranked.first().expect("a rule claims the provisional");
    let out = (rule.handle)(&ctx).expect("the rule handles it");
    (rule.id, out.actions)
}

/// The actions the first ranked default rule produces for `event` from `b-1`.
fn handle_from_b1(call: &call::Call, event: &CallEvent) -> (&'static str, Vec<RuleAction>) {
    handle_from(call, event, "b-1")
}

/// One executor turn over `call` for `actions`, in the context of `event`
/// arriving from `b-1`.
fn execute_from_b1(call: &call::Call, event: &CallEvent, actions: &[RuleAction]) -> HandlerResult {
    let config = B2buaConfig::default();
    let ctx = RuleContext {
        call: RuleCall::new(call),
        call_ref: &call.call_ref,
        event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &config,
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    exec.execute(actions, call, &ctx)
}

/// The requests `result` sent toward `leg` with `method`.
fn requests_to(result: &HandlerResult, leg: &str, method: &str) -> usize {
    result
        .effects
        .outbound
        .iter()
        .filter(|e| e.leg_id.as_deref() == Some(leg))
        .filter(
            |e| matches!(&e.body, b2bua_core::effects::OutboundBody::Request(r) if r.method() == method),
        )
        .count()
}

#[test]
fn a_provisional_under_an_answered_caller_is_absorbed_with_what_the_leg_is_owed() {
    // The caller's INVITE carries its 200; a b-leg dialled later rings. The
    // relay rule relays nothing toward her and still gives the ringing leg
    // its due: its early dialog (a teardown then CANCELs it) and the CDR event.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    call = call::helpers::add_b_leg(call, b_leg_pending());

    let (id, actions) = handle_from_b1(&call, &b_leg_ringing_event(false));
    assert_eq!(id, "relay-provisional");
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::RelayToPeer { .. })),
        "nothing is relayed toward the answered caller: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            RuleAction::TrackEarlyDialog { leg_id, .. } if leg_id == "b-1"
        )),
        "the ringing leg's early dialog is tracked: {actions:?}"
    );
    assert!(
        actions.iter().any(|a| matches!(
            a,
            RuleAction::AddCdrEvent { event_type: call::CdrEventType::Provisional, leg_id, status_code: Some(180), .. }
                if leg_id == "b-1"
        )),
        "the provisional is accounted: {actions:?}"
    );
    // …and the early dialog the provisional establishes (RFC 3261 §12.1.2):
    // the callee's tag, its Contact as the remote target, the recorded route.
    let result = execute_from_b1(&call, &b_leg_ringing_event(false), &actions);
    let dialog = result.call.b_legs[0]
        .dialogs
        .by_tags("svc", "bobtag")
        .expect("the early dialog carries the callee's tag");
    assert_eq!(dialog.state, DialogState::Early);
    assert!(
        dialog.remote_target.contains(CALLEE_RINGING_CONTACT),
        "the remote target is the provisional's Contact: {}",
        dialog.remote_target
    );
    assert!(
        dialog.route_set.iter().any(|r| r.contains(CALLEE_RINGING_ROUTE)),
        "the route set is the provisional's recorded route: {:?}",
        dialog.route_set
    );
    assert_eq!(result.call.b_legs[0].state(), LegState::Early);
}

#[test]
fn a_bare_180_after_the_final_mints_no_mask_state() {
    // The bare-180 downgrade asked to ring an answered caller: refused at the
    // seam before the mask's own bookkeeping — no a-facing tag mapping, the
    // mask still unrelayed.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    call = call::helpers::add_b_leg(call, b_leg_pending());
    let event = b_leg_ringing_event(false);

    let result = execute_from_b1(
        &call,
        &event,
        &[RuleAction::RelayFirstBare180 { leg_id: "b-1".into(), b_tag: "bobtag".into() }],
    );
    assert!(provisionals_to_a(&result).is_empty(), "no 180 leaves: {:?}", result.effects.outbound);
    assert_eq!(late_provisional_refusals(&result), vec![(180, 200)]);
    let fronts: Vec<_> = result.call.a_leg.dialogs.iter().flat_map(|d| d.fronts.iter()).collect();
    assert!(fronts.is_empty(), "no caller-facing dialog fronts it: {fronts:?}");
    assert!(
        !call::helpers::relay_first_18x_first_relayed(&result.call),
        "the mask has relayed nothing"
    );
}

#[test]
fn a_repeated_transfer_target_provisional_is_accounted_once() {
    // The REFER machine's C leg repeats a 180 the referrer was already
    // notified of: the NOTIFY is deduped and so is the CDR event — the leg's
    // due (Early) still rides.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    let mut referrer = b_leg_pending();
    call::testkit::stage_leg(&mut referrer, LegState::Confirmed);
    with_dialog(&mut referrer, "reftag");
    call = call::helpers::add_b_leg(call, referrer);
    let mut target = b_leg_pending();
    target.leg_id = "b-2".into();
    call = call::helpers::add_b_leg(call, target);
    call.transfer = Some(call::TransferState {
        phase: call::TransferPhase::CRinging,
        referrer_leg_id: "b-1".into(),
        refer_to_uri: "sip:carol@10.0.0.3".into(),
        effective_refer_to_uri: None,
        callback_context: None,
        c_leg_id: Some("b-2".into()),
        refer_cseq: Some(2),
        started_at_ms: 0,
        last_c_leg_provisional_status: Some(180),
        c_initial_sdp: None,
        subscription_terminated: false,
    });
    b2bua_core::rules::refer_transfer::project_cursor(&mut call);

    let (id, actions) = handle_from(&call, &b_leg_ringing_event(false), "b-2");
    assert_eq!(id, "transfer-c-1xx-absorb");
    let accounted = actions
        .iter()
        .filter(|a| {
            matches!(a, RuleAction::AddCdrEvent { event_type: call::CdrEventType::Provisional, .. })
        })
        .count();
    assert_eq!(accounted, 0, "a repeat of a status is not accounted again: {actions:?}");
    assert!(
        !actions.iter().any(|a| matches!(a, RuleAction::SendNotify { .. })),
        "the repeat draws no NOTIFY: {actions:?}"
    );
}

#[test]
fn a_provisional_on_a_confirmed_b_leg_selects_no_relay_rule() {
    // A 1xx after the leg's own 2xx (a callee's late provisional, a fork the
    // proxy failed to cancel, a retransmission) answers a transaction that
    // took its final: no relay rule claims it, so nothing moves the leg back
    // to Early or rings the caller.
    let mut call = call::helpers::record_invite_final(test_call(), "a", 200);
    confirm_a(&mut call);
    let mut confirmed = b_leg_pending();
    call::testkit::stage_leg(&mut confirmed, LegState::Confirmed);
    with_dialog(&mut confirmed, "bobtag");
    call = call::helpers::add_b_leg(call, confirmed);

    let event = b_leg_ringing_event(false);
    let ctx = RuleContext {
        call: RuleCall::new(&call),
        call_ref: &call.call_ref,
        event: &event,
        source_leg_id: "b-1",
        direction: Direction::FromB,
        now_ms: 0,
        config: &B2buaConfig::default(),
        discharged: None,
        resolved: &call::helpers::UNRESOLVED,
    };
    let rules = default_rules();
    let ranked: Vec<&str> = pick_ranked(&rules, &call, &ctx).iter().map(|r| r.id).collect();
    assert!(ranked.is_empty(), "a provisional on a confirmed leg selects no rule, got {ranked:?}");
}

#[test]
fn begin_termination_resolves_an_a_leg_whose_invite_carries_its_final() {
    // The `Cancelled` turn: sip-txn sent the 487 and the record carries that
    // final, so the a-leg reads Terminated — resolved, never re-answered —
    // and `BeginTermination` leaves it so.
    let mut call = test_call();
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    let call = call::helpers::record_invite_final(call, "a", 487);
    assert_eq!(call.a_leg.state(), LegState::Terminated);
    let result = execute_on(
        &call,
        &[RuleAction::BeginTermination {
            reason: Some("CANCEL".into()),
            cause: TerminationCause::RemoteCancel,
            by_leg: Some("a".into()),
        }],
    );
    assert_eq!(result.call.a_leg.state(), LegState::Terminated);
    assert_eq!(result.call.a_leg.end(), Some(call::ByeDisposition::Rejected));
    assert!(result.call.a_leg.is_resolved());

    // An a-leg with NO final yet stays Early: the ADR-0022 funnel still owes
    // that caller its 503 at `→ terminated`.
    let mut call = test_call();
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    let result = execute_on(
        &call,
        &[RuleAction::BeginTermination {
            reason: Some("BYE".into()),
            cause: TerminationCause::RemoteBye,
            by_leg: Some("a".into()),
        }],
    );
    assert_eq!(result.call.a_leg.state(), LegState::Early);
}

// ── ADR-0016: global call machine projection ────────────────────────────────

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
    confirm_a(&mut call);
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(r.call.state, CallModelState::Terminating, "not promoted while unresolved");
    assert_eq!(cursor(&r).as_deref(), Some("Terminating"));

    // Terminated → "Terminated".
    call.state = CallModelState::Terminated;
    let r = invariants::finalize(HandlerResult::new(call.clone()));
    assert_eq!(cursor(&r).as_deref(), Some("Terminated"));
}

// ── ADR-0016: machine-gated selection + SetState + transition check ─────────

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
        resolved: &call::helpers::UNRESOLVED,
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
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let rules = vec![sm_rule(handle_to_s1)];

    let result = {
        let ctx = ctx_for(&call, &event, &config);
        execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core())
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

/// The turn goes on; the defect is the turn's to report.
#[test]
fn undeclared_transition_is_a_noted_defect() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let rules = vec![sm_rule(handle_to_s2_undeclared)];

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert_eq!(defect_names(&exec), [b2bua_core::turn_notes::UNDECLARED_TRANSITION]);
}

/// The names of the defects `exec`'s turn noted.
fn defect_names(exec: &ActionExecutor) -> Vec<&'static str> {
    exec.turn.notes.defects().iter().map(|d| d.name).collect()
}

/// A tracked side effect the handler emits but the rule did not declare is
/// the drift-check's defect (ADR-0016 X9), the effect analogue of the
/// transition check.
#[test]
fn undeclared_effect_is_a_noted_defect() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    // `sm_rule` declares `effects: &[]`, but the handler emits a `LegMessage`.
    let rules = vec![sm_rule(handle_emits_leg_message)];

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert_eq!(defect_names(&exec), [b2bua_core::turn_notes::UNDECLARED_EFFECT]);
}

/// Declaring the matching `LegMessage` effect satisfies the drift-check — the
/// same emit no longer trips it (the by-category `emitted ⊆ declared` contract).
#[test]
fn declared_effect_passes_the_drift_check() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let rules = vec![sm_rule_with_effects(handle_emits_leg_message, &SM_EFFECTS_LEG_MESSAGE)];

    let ctx = ctx_for(&call, &event, &config);
    // No defect: the emitted Respond is a declared LegMessage.
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert!(defect_names(&exec).is_empty());
}

/// `ClearState` (machine deactivation) removes the cursor, and with a declared
/// `S0 => terminal` edge the transition drift-check accepts it (ADR-0016 X9).
#[test]
fn declared_terminal_clear_state_deactivates_machine() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let rules = vec![sm_rule_with_transitions(handle_clears_state, &SM_TRANSITIONS_TERMINAL)];

    let ctx = ctx_for(&call, &event, &config);
    let r = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    // The machine is deactivated: its cursor is gone.
    assert!(!r.call.sm_cursors.contains_key(&MachineId::new(TEST_MACHINE)));
}

/// A `ClearState` whose `S0 => terminal` edge is **not** declared is the
/// transition drift-check's defect (only `S0 => S1` is in `SM_TRANSITIONS`).
#[test]
fn undeclared_terminal_clear_state_is_a_noted_defect() {
    let mut call = test_call();
    call.sm_cursors.insert(MachineId::new(TEST_MACHINE), StateLabel::new("S0"));
    let event = info_event();
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let rules = vec![sm_rule(handle_clears_state)]; // transitions: S0 => S1 only

    let ctx = ctx_for(&call, &event, &config);
    let _ = execute_rules(&rules, &call, &ctx, &mut exec, &ObligationSet::core());
    assert_eq!(defect_names(&exec), [b2bua_core::turn_notes::UNDECLARED_TRANSITION]);
}

// ── b-leg route-set capture (RFC 3261 §12.1.2) ──────────────────────────────
//
// The b-leg is a UAC dialog, so `ConfirmDialog` must build its route set from
// the dialog-creating 2xx's Record-Route headers in *reverse* order. Without
// this the route set stays empty and every worker→callee in-dialog request
// (keepalive OPTIONS, BYE, re-INVITE) falls back to the synthetic
// `b2b_outbound_proxy` Route in `relay::apply_b_leg_egress`, silently dropping
// the proxy's signed Record-Route cookie. The transparent-failover matrix
// routes identically with or without this capture (the proxy classifies the
// request `;outbound` and ignores the cookie either way), so the only place the
// omission shows is the Route header content asserted here.

/// A pending (Trying) b-leg dialled under `svc`, no response yet so no
/// dialog — the state `ConfirmDialog` mutates when the 2xx arrives.
fn b_leg_pending() -> Leg {
    Leg {
        leg_id: "b-1".into(),
        call_id: "bcid@x".into(),
        source: RemoteInfo { address: "10.0.0.2".into(), port: 5070 },
        dialogs: Default::default(),
        local_uri: Some("sip:svc@10.0.0.9".into()),
        remote_uri: Some("sip:bob@10.0.0.2".into()),
        invite_request_uri: Some("sip:bob@10.0.0.2:5070".into()),
        outbound_invite: Some(OutboundInvite::new("svc", 1, 0, None)),
        own_tag: None,
        ext: None,
        kind: Some(LegKind::Destination),
        adopted: None,
        invite_final_sent: None,
        answer_preempted: None,
        attempt: None,
        ring: call::Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: Default::default(),
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);

    let d = result.call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog");
    assert_eq!(
        d.route_set,
        vec![
            "<sip:proxy-a.example:5060;lr>".to_string(),
            "<sip:proxy-b.example:5060;lr>".to_string(),
        ],
        "b-leg route set must be the 2xx Record-Route in reverse order (§12.1.2)"
    );
    // The other 2xx-learned fields stay correct alongside the new capture.
    assert_eq!(d.peer_tag, "bobtag");
    assert_eq!(d.remote_target, "sip:bob@10.0.0.2:5070");
}

// The front proxy's two Record-Route halves (cookie + `;outbound`) arrive on the
// wire COMBINED in a single Record-Route header (RFC 3261 §7.3.1). The §12.1.2
// reversal must operate on individual route URIs, not header lines — otherwise
// reversing one combined value is a no-op and leaves the cookie on top, so the
// worker→callee keepalive carries the cookie first and the proxy bounces it back
// to a worker after a reboot (no `;outbound` rescue). The b-leg route set MUST end up `;outbound`
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);
    let rs = &result.call.b_legs[0].dialogs.confirmed().expect("the confirmed dialog").route_set;
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };

    let result = exec.execute(&[RuleAction::ConfirmDialog { leg_id: "b-1".into() }], &call, &ctx);

    assert!(
        result.call.b_legs[0]
            .dialogs
            .confirmed()
            .expect("the confirmed dialog")
            .route_set
            .is_empty(),
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
// correlation Via branch echoed verbatim. A CANCEL sent to `leg.source` or
// without the Route bypasses the proxy and never reaches the pending server txn.
#[test]
fn cancel_follows_invite_route_set_and_next_hop_through_the_outbound_proxy() {
    use b2bua_core::effects::OutboundBody;

    let config = B2buaConfig {
        b2b_outbound_proxy: Some(("proxy.example".to_string(), 5060)),
        ..B2buaConfig::default()
    };

    let call = test_call();
    let a_invite = crate::caller_of(&call);
    let mut id_gen = scope(7);
    // The callee's own address — what `leg.source` becomes; the CANCEL must NOT
    // go here (it must go to the proxy).
    let callee_dest = ("10.0.0.2".to_string(), 5070u16);
    let (leg, invite_effect) = b2bua_core::rules::relay::build_b_leg(
        b2bua_core::rules::relay::CallMarks::of(&call),
        "b-1",
        &a_invite,
        callee_dest.clone(),
        None,
        None,
        None,
        &config,
        &mut id_gen.ids,
        None,
        &[],
        &CapabilitySet::default(),
        None, // no charging vector
        &[],
        &[], // no withheld option tags
        None,
        true,
        0,
        &mut Vec::new(),
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
        key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
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
        resolved: &call::helpers::UNRESOLVED,
    };
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
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

// ── Media/INFO primitives + leg-kind relay gate (ADR-0016) ───────────────────
//
// An unadopted `media` leg is gated out of the generic relay-to-peer `→ a`
// fallback, while an adopted leg still relays; `send-provisional-to-leg` brokers
// a 183 early-media onto the a-leg; and `send-request-to-leg` carries an opaque
// MSCML INFO body to a named leg.
mod media_primitives {
    use super::*;
    use b2bua_core::effects::OutboundBody;
    use b2bua_core::rules::MessageTransform;
    use sip_message::HeaderName;
    mod unbuilt_relay;

    /// A confirmed b-leg of the given role; its single dialog carries the callee
    /// tag so in-dialog originators have a confirmed dialog to ride.
    fn confirmed_b_leg(leg_id: &str, kind: LegKind) -> Leg {
        let mut leg = super::b_leg_pending();
        leg.leg_id = leg_id.into();
        call::testkit::stage_leg(&mut leg, LegState::Confirmed);
        leg.kind = Some(kind);
        leg.adopted = None; // derive adoption from the kind
        with_dialog(&mut leg, "bobtag").peer_cseq = Some(1);
        leg
    }

    /// The a-leg with a confirmed dialog, the target a relayed request toward A rides.
    fn give_a_leg_dialog(call: &mut call::Call) {
        confirm_a(call);
        call.a_leg.dialogs = vec![a_dialog(call, "a-svc", DialogState::Confirmed)].into();
        call.a_leg.own_tag = Some("a-svc".into());
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
        id_gen: &'a mut TurnScope,
        actions: &[RuleAction],
    ) -> HandlerResult {
        let mut exec = ActionExecutor { config, turn: id_gen, now_ms: 0 };
        let ctx = RuleContext {
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event,
            source_leg_id,
            direction: Direction::FromB,
            now_ms: 0,
            config,
            discharged: None,
            resolved: &call::helpers::UNRESOLVED,
        };
        exec.execute(actions, call, &ctx)
    }

    /// The statuses `result` answered with, in order.
    fn answered(result: &HandlerResult) -> Vec<u16> {
        result
            .effects
            .outbound
            .iter()
            .filter_map(|e| match &e.body {
                b2bua_core::effects::OutboundBody::Response(r) => Some(r.status()),
                _ => None,
            })
            .collect()
    }

    // A caller request toward a peer dialog this stack's BYE is ending draws
    // the final its method always takes where no relay could ever carry it
    // (SUBSCRIBE: 489, RFC 6665 §4.2.1.2), and 500 + Retry-After where a
    // relay could later (INFO, RFC 3261 §14.2).
    #[test]
    fn a_request_toward_an_ending_peer_takes_its_methods_final() {
        let mut call = test_call();
        give_a_leg_dialog(&mut call);
        let mut b = confirmed_b_leg("b-1", LegKind::Destination);
        for d in b.dialogs.iter_mut() {
            d.state = DialogState::Ending(call::EndingBy::Bye);
        }
        call = call::helpers::add_b_leg(call, b);
        call = call::helpers::merge_leg(call, "a", "b-1");
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        for (method, status) in
            [(sip_message::Method::Subscribe, 489), (sip_message::Method::Info, 500)]
        {
            let event = CallEvent::Sip {
                message: Box::new(SipMessage::Request(super::in_dialog_request(method.clone()))),
                src: "127.0.0.1:5060".parse().unwrap(),
                matched_client_txn: false,
            };
            let result = exec_on(
                &call,
                &event,
                "a",
                &config,
                &mut id_gen,
                &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
            );
            assert_eq!(answered(&result), vec![status], "{method:?}");
        }
    }

    // Relay-to-peer from an unadopted media leg relays nothing — it must NOT
    // fall back to A — and its request is refused 403.
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
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::RelayToPeer { transform: MessageTransform::default() }],
        );
        let sent: Vec<Option<u16>> = result
            .effects
            .outbound
            .iter()
            .map(|e| match &e.body {
                b2bua_core::effects::OutboundBody::Response(r) => Some(r.status()),
                _ => None,
            })
            .collect();
        assert_eq!(
            sent,
            vec![Some(403)],
            "an unadopted media leg's relay-to-peer is refused 403, never mis-routed to A"
        );
    }

    // An adopted (destination) leg still falls back to A — the gate only
    // suppresses unadopted legs.
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
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
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
        let mut id_gen = scope(1);
        let mscml = b"<MediaServerControl><request><play/></request></MediaServerControl>".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::SendRequestToLeg {
                leg_id: "b-1".into(),
                method: "INFO".into(),
                body: Some(Body::new(
                    mscml.clone(),
                    Some("application/mediaservercontrol+xml".into()),
                    BodyAuthor::Stack,
                )),
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
    // headers verbatim onto the request — the seam a deferred User-to-User relay uses
    // to carry a held `User-To-User` toward the peer at an async decision re-entry.
    // Body-owned headers (Content-Type/Content-Length) listed
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
        let mut id_gen = scope(1);
        let uui = "3030373946313233;encoding=hex"; // RFC 7433 User-To-User
        let body = b"SUP:example-binary\x00\x01payload".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::SendRequestToLeg {
                leg_id: "b-1".into(),
                method: "INFO".into(),
                body: Some(Body::new(
                    body.clone(),
                    Some("application/example-binary".into()),
                    BodyAuthor::Stack,
                )),
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
        let mut id_gen = scope(1);
        let sdp = b"v=0\r\no=mrf 1 1 IN IP4 10.0.0.50\r\n".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::SendProvisionalToLeg {
                leg_id: "a".into(),
                status: 183,
                reason: "Session Progress".into(),
                body: Some(Body::new(sdp.clone(), None, BodyAuthor::Stack)),
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
            result.call.a_leg.own_tag.as_deref().is_some_and(|t| {
                !t.is_empty() && call::helpers::caller_dialog(&result.call, t).is_some()
            }),
            "the a-facing early tag is persisted"
        );
    }

    // A non-1xx status (or a non-a target) is rejected — no UAS transaction to
    // answer.
    #[test]
    fn send_provisional_rejects_non_provisional_status() {
        let call = test_call();
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(in_dialog_info())),
            src: "10.0.0.2:5070".parse().unwrap(),
            matched_client_txn: false,
        };
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "a",
            &config,
            &mut id_gen,
            &[RuleAction::SendProvisionalToLeg {
                leg_id: "a".into(),
                status: 200,
                reason: "OK".into(),
                body: None,
                to_tag: None,
                p_early_media: None,
            }],
        );
        assert!(result.effects.outbound.is_empty(), "a non-1xx provisional is rejected");
    }
}

// ── AnswerALegNewDialog: a-side fork-confirm (RFC 3261 §12.1 / RFC 3264 §5.1) ──
//
// An early-media callflow answers ONE caller INVITE in two stages with two
// *different* To-tags: 183 (media-server SDP, tag A1) then 200 (callee SDP, tag
// A2 ≠ A1). `AnswerALegNewDialog` mints/adopts A2, supersedes the early a-dialog
// A1, relays the callee 200/SDP under A2, and confirms the a-leg.
mod answer_a_leg_new_dialog {
    use super::*;
    use b2bua_core::effects::OutboundBody;
    use b2bua_core::rules::RelayedFinal;
    use sip_message::generators::SourceBody;
    use sip_message::HeaderName;

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
        id_gen: &'a mut TurnScope,
        actions: &[RuleAction],
    ) -> HandlerResult {
        let mut exec = ActionExecutor { config, turn: id_gen, now_ms: 0 };
        let ctx = RuleContext {
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event,
            source_leg_id,
            direction: Direction::FromB,
            now_ms: 0,
            config,
            discharged: None,
            resolved: &call::helpers::UNRESOLVED,
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
        let mut id_gen = scope(1);
        let sdp_b = b"v=0\r\no=callee 2 2 IN IP4 10.0.0.70\r\n".to_vec();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: Some(Body::from_leg(sdp_b.clone(), "b-1")),
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
        let d = result.call.a_leg.dialogs.confirmed().expect("a confirmed a-dialog");
        assert_eq!(d.our_tag, a2, "the confirmed a-dialog is A2's");
        assert!(
            result
                .call
                .a_leg
                .dialogs
                .by_tags("A1early", &d.peer_tag)
                .is_some_and(|e| e.retired().is_some()),
            "the early-media dialog A1 is retired, not renamed"
        );
        assert_eq!(
            d.cached_sdp.as_deref(),
            Some(sdp_b.as_slice()),
            "the answer SDP is cached for a §13.3.1.4 un-ACKed-2xx retransmit"
        );
        assert_eq!(result.call.a_leg.state(), LegState::Confirmed, "the a-leg is confirmed");
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
        call::testkit::stage_leg(&mut b, LegState::Confirmed);
        with_dialog(&mut b, "bobtag").pending_reinvite_2xx = Some(call::Unacked2xx {
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
        call.provisionals.push(call::testkit::shown_provisional(
            "b-1",
            "svc",
            1,
            9,
            "A1early",
            1,
            call::Ladder::Repeating(
                call::RetainedEmission::paced(
                    b"SIP/2.0 183 Session Progress\r\n\r\n".to_vec(),
                    ("10.0.0.2".into(), 5070),
                    sip_retransmit::Class::ReliableProvisional,
                    call::Repeated::response("INVITE", 183),
                )
                .0,
            ),
            call::ResponderAck::RelayedPrack,
        ));
        let provisional =
            call::Obligation::AwaitPrack { tag: "A1early".into(), rseq: call::RSeq(1) };
        for (key, fire_at) in [
            (TimerKey::obligation(TimerType::Rung, reinvite_2xx.clone()), 500),
            (TimerKey::obligation(TimerType::RepeatGiveUp, reinvite_2xx.clone()), 32_000),
            (TimerKey::obligation(TimerType::Rung, provisional.clone()), 500),
        ] {
            call::helpers::arm_timer(&mut call, key, fire_at);
        }

        let event = some_event();
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: None,
                to_tag: None,
                header_updates: vec![],
                relayed: RelayedFinal::none(),
            }],
        );

        let b_dialog = result.call.b_legs[0].dialogs.confirmed().expect("the b-leg dialog");
        assert!(
            b_dialog.pending_reinvite_2xx.is_some(),
            "the b-leg's re-INVITE 2xx still awaits its ACK: marker kept",
        );
        let ladder_timers = |o: &call::Obligation| {
            result.call.timers.iter().filter(|t| t.key.obligation_of() == Some(o)).count()
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
        assert_eq!(
            result.call.provisionals[0].shown.as_ref().map(|s| &s.ladder),
            Some(&call::Ladder::Ceased(call::Ceased::Final)),
            "the provisional repeats nothing more",
        );
        let cancelled: Vec<String> = result
            .effects
            .critical
            .iter()
            .filter_map(|e| match e {
                CriticalStateEffect::CancelTimer { key } => Some(format!("{key:?}")),
                _ => None,
            })
            .collect();
        assert_eq!(
            cancelled,
            vec!["Rung on AwaitPrack:A1early:1"],
            "only the provisional's timers leave the driver",
        );
        assert!(
            result.call.a_leg.dialogs.confirmed().is_some_and(|d| d.answered_2xx.is_some()),
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
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: None,
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
            result.call.a_leg.dialogs.confirmed().map(|d| d.our_tag.as_str()),
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
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 486,
                reason: "Busy Here".into(),
                body: None,
                to_tag: None,
                header_updates: vec![],
                relayed: RelayedFinal::none(),
            }],
        );
        assert!(result.effects.outbound.is_empty(), "a non-2xx final establishes no a-dialog");
        let a1 = result.call.a_leg.dialogs.iter().map(|d| (d.our_tag.as_str(), &d.state));
        assert_eq!(
            a1.collect::<Vec<_>>(),
            vec![("A1early", &DialogState::Early)],
            "the early dialog A1 is left untouched",
        );
    }

    // The fork-confirm 2xx is an a-facing final the B2BUA answers on a leg of
    // its own: with no callee response to relay an advertisement from and no
    // declaration, it states none.
    #[test]
    fn states_no_capability_advert_unless_declared() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: None,
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
    // absent.
    #[test]
    fn header_updates_override_the_capability_advert() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: None,
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
    /// of its own, a vendor header, its capability advert and session interval,
    /// and lines the stack owns.
    fn callee_final() -> sip_message::SipResponse {
        let raw = "SIP/2.0 200 OK\r\n\
Via: SIP/2.0/UDP 10.0.0.70:5060;branch=z9hG4bK-b\r\n\
From: <sip:alice@host>;tag=alice-tag\r\n\
To: <sip:callee@host>;tag=callee-tag\r\n\
Call-ID: b-leg\r\n\
CSeq: 1 INVITE\r\n\
Contact: <sip:callee@10.0.0.70:5060>\r\n\
Privacy: none\r\n\
X-Vendor-Id: 100000042\r\n\
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
    /// structural set and the callee's Contact stay behind, and the callee's
    /// advert is the caller's to see where the face declares none.
    #[test]
    fn relays_the_delivered_final_onto_the_answer() {
        let call = call_with_early_a_dialog("A1early");
        let event = some_event();
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        let delivered = callee_final();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: Some(Body::from_leg(delivered.body().to_vec(), "b-1")),
                to_tag: None,
                header_updates: vec![],
                relayed: RelayedFinal::of(
                    &delivered,
                    &call.a_leg_invite,
                    200,
                    SourceBody::Verbatim,
                    &B2buaConfig::default(),
                ),
            }],
        );
        match &result.effects.outbound[0].body {
            OutboundBody::Response(r) => {
                assert_eq!(r.raw(HeaderName::from("Privacy")).next(), Some("none"));
                assert_eq!(r.raw(HeaderName::from("X-Vendor-Id")).next(), Some("100000042"));
                assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).next(), Some("opaque-42"));
                assert_eq!(
                    r.raw(HeaderName::Allow).next(),
                    Some("INVITE, ACK, BYE"),
                    "an undeclared half states what the delivered final advertised"
                );
                assert_eq!(r.raw(HeaderName::Allow).count(), 1);
                assert_eq!(
                    r.raw(HeaderName::SessionExpires).next(),
                    Some("1800;refresher=uas"),
                    "the session-timer negotiation rides end to end (RFC 4028)"
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
                assert_eq!(r.to().tag(), result.call.a_leg.own_tag.as_deref());
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
        let mut id_gen = scope(1);
        let delivered = callee_final();
        let result = exec_on(
            &call,
            &event,
            "b-1",
            &config,
            &mut id_gen,
            &[RuleAction::AnswerALegNewDialog {
                status: 200,
                reason: "OK".into(),
                body: Some(Body::from_leg(delivered.body().to_vec(), "b-1")),
                to_tag: None,
                header_updates: vec![
                    ("X-Vendor-Thing".into(), Some("service-owned".into())),
                    ("X-Vendor-Id".into(), None),
                    ("Allow".into(), Some("INVITE".into())),
                ],
                relayed: RelayedFinal::of(
                    &delivered,
                    &call.a_leg_invite,
                    200,
                    SourceBody::Verbatim,
                    &B2buaConfig::default(),
                ),
            }],
        );
        match &result.effects.outbound[0].body {
            OutboundBody::Response(r) => {
                assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).next(), Some("service-owned"));
                assert_eq!(r.raw(HeaderName::from("X-Vendor-Thing")).count(), 1);
                assert_eq!(
                    r.raw(HeaderName::from("X-Vendor-Id")).next(),
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
// ACK (no body, no Content-Type). The body is a binary-safe `Vec<u8>`. Every
// emitted ACK retains its exact datagram on the dialog (`emitted_ack`), and a
// bodyless AckLeg re-passes those bytes raw (§13.2.2.4 — the re-ACK is THE ACK
// the 2xx triggered).
mod ack_leg_body {
    use super::*;
    use b2bua_core::effects::OutboundBody;
    use sip_message::HeaderName;

    /// A confirmed b-leg (callee tag learned) whose first dialog `AckLeg` addresses.
    fn b_leg_confirmed() -> Leg {
        let mut leg = b_leg_pending();
        call::testkit::stage_leg(&mut leg, LegState::Confirmed);
        with_dialog(&mut leg, "bobtag");
        leg
    }

    /// Execute one `AckLeg { leg_id: "b-1", body, content_type }` against a call
    /// carrying a confirmed `b-1`, and return the emitted ACK. `AckLeg` reads only
    /// the call + the action fields (not the event), so any event serves.
    fn ack_request(body: Vec<u8>, content_type: Option<String>) -> SipRequest {
        let call = call::helpers::add_b_leg(test_call(), b_leg_confirmed());
        let config = B2buaConfig::default();
        let event = CallEvent::Timer {
            key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
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
            resolved: &call::helpers::UNRESOLVED,
        };
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        let result = exec.execute(
            &[RuleAction::AckLeg {
                leg_id: "b-1".into(),
                body: Some(Body::new(body, content_type, BodyAuthor::Stack)),
            }],
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

    // An empty body sends a bare ACK with no Content-Type.
    #[test]
    fn ack_leg_empty_body_sends_bare_ack_with_no_content_type() {
        let ack = ack_request(Vec::new(), None);
        assert!(ack.body().is_empty(), "an empty AckLeg body sends a bodyless ACK");
        assert_eq!(
            ack.raw(HeaderName::ContentType).next(),
            None,
            "a bare ACK carries no Content-Type"
        );
    }

    // ── The retained ACK datagram (§13.2.2.4 re-ACK: THE ack, re-passed) ─────

    /// Execute one `AckLeg { leg_id: "b-1", body, content_type }` against `call`
    /// and return the full [`HandlerResult`] (these tests read both the wire
    /// effect and the updated call).
    fn ack_exec(call: &call::Call, body: Vec<u8>, content_type: Option<String>) -> HandlerResult {
        let config = B2buaConfig::default();
        let event = CallEvent::Timer {
            key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
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
            resolved: &call::helpers::UNRESOLVED,
        };
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        exec.execute(
            &[RuleAction::AckLeg {
                leg_id: "b-1".into(),
                body: Some(Body::new(body, content_type, BodyAuthor::Stack)),
            }],
            call,
            &ctx,
        )
    }

    /// The one emitted ACK effect in `result`.
    fn ack_effect(result: &HandlerResult) -> &b2bua_core::effects::OutboundSipEffect {
        result
            .effects
            .outbound
            .iter()
            .find(|e| matches!(&e.body, OutboundBody::Request(r) if r.method() == "ACK"))
            .expect("AckLeg emits an ACK request")
    }

    /// The one re-ACK in `result`: a retained datagram repeated as its bytes
    /// (RFC 3261 §13.2.2.4), never a composed request.
    fn re_ack_effect(result: &HandlerResult) -> &b2bua_core::effects::OutboundSipEffect {
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
            .confirmed()
            .and_then(|d| d.emitted_ack.clone())
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
            .confirmed()
            .and_then(|d| d.emitted_ack.clone())
            .expect("the bare ACK is retained as well");
        assert_eq!(retained.wire().0, sent.image());
        assert!(sent.body().is_empty(), "a minted ACK stays bare");

        let second = ack_exec(&first.call, Vec::new(), None);
        let OutboundBody::Datagram(re_sent) = &re_ack_effect(&second).body else { unreachable!() };
        assert_eq!(re_sent.wire().0, sent.image(), "the re-ACK repeats the bare datagram");
    }
}

mod create_leg_admission;

mod txn_timeouts;

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
    use b2bua_core::effects::OutboundBody;
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
            resolved: &call::helpers::UNRESOLVED,
        };
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };

        // The service sources the fake offer from the config parameter — the whole
        // opt-in wiring. (A normal reroute passes `body_override: None` here, which
        // relays the caller's own offer; `default_sdp` never substitutes itself.)
        let create = RuleAction::CreateLeg {
            destination: ("10.0.1.5".into(), 5070), // IP literal → admission passes
            new_ruri: None,
            new_from: None,
            new_to: None,
            callback_context: None,
            body_override: config.default_sdp.clone().map(|sdp| Body::own(sdp, None)),
            header_updates: vec![],
            header_adds: vec![],
            kind: None,
            attempt: None,
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
    use b2bua_core::effects::OutboundBody;
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
                SipHeader { name: "X-Vendor-Term".into(), value: "sbc.example".into() },
                SipHeader { name: "P-Kept".into(), value: "rides-on".into() },
            ],
        };
        generate_out_of_dialog_request(OutOfDialogMethod::Invite, &opts).expect("an INVITE")
    }

    /// The b-leg INVITE a `CreateLeg` carrying `header_updates` emits.
    fn b_leg_invite(header_updates: Vec<(String, Option<String>)>) -> SipRequest {
        let config = B2buaConfig::default();
        let a_invite = invite_with_vendor_header();
        let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
        let call = build_initial_call(&a_invite, src, &config, &mut sip_ids::TurnIds::fixed(1), 0);
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
            resolved: &call::helpers::UNRESOLVED,
        };
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        let create = RuleAction::CreateLeg {
            destination: ("10.0.1.5".into(), 5070), // IP literal → admission passes
            new_ruri: None,
            new_from: None,
            new_to: None,
            callback_context: None,
            body_override: None,
            header_updates,
            header_adds: vec![],
            kind: None,
            attempt: None,
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
        assert_eq!(inv.raw(HeaderName::from("X-Vendor-Term")).next(), Some("sbc.example"));
        assert_eq!(inv.raw(HeaderName::from("P-Kept")).next(), Some("rides-on"));
    }

    #[test]
    fn a_removal_withholds_the_name_and_leaves_every_other_relayed_header() {
        let inv = b_leg_invite(vec![("X-Vendor-Term".into(), None)]);
        assert_eq!(
            inv.raw(HeaderName::from("X-Vendor-Term")).next(),
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
        let inv = b_leg_invite(vec![("x-VENDOR-term".into(), None)]);
        assert_eq!(inv.raw(HeaderName::from("X-Vendor-Term")).next(), None);
    }
}

// ── ADR-0020 X7: obligation-set equivalence gate ────────────────────────────
//
// Over arbitrary call snapshots (states × limiter state incl. fail-open ×
// pre-emitted effects), `invariants::enforce(&ObligationSet::core(), …)` produces
// the same call and effect lanes as `reference_enforce`, which states the
// limiter and CDR obligations inline (its limiter dedupe counted per hold).
mod enforce_equivalence {
    use super::*;
    use b2bua_core::effects::{
        BufferedObservabilityEffect, CriticalStateEffect, HandlerEffects, SoftBoundedEffect,
    };
    use b2bua_core::obligations::ObligationSet;
    use call::{CallLimiterState, TimerEntry};
    use proptest::prelude::*;

    /// The limiter and CDR obligations written inline (ADR-0010 X5 shape).
    fn reference_enforce(before: &call::Call, mut result: HandlerResult) -> HandlerResult {
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

        // One release per call that sent an admit: a pre-emitted release
        // discharges it.
        let already =
            result.effects.soft.iter().any(
                |e| matches!(e, SoftBoundedEffect::ReleaseLimiter { key } if *key == result.call.limiter.key()),
            );
        if result.call.limiter.owed_release().is_some() && !already {
            result.effects.soft.push(SoftBoundedEffect::ReleaseLimiter {
                key: result.call.limiter.key().to_string(),
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

    fn arb_limiter_state() -> impl Strategy<Value = CallLimiterState> {
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            proptest::collection::vec(prop_oneof![Just("l1"), Just("l2")], 0..3),
        )
            .prop_map(|(counted, sent, fail_open, ids)| {
                let entries: Vec<call::LimiterEntry> = ids
                    .into_iter()
                    .map(|id| call::LimiterEntry { id: id.to_string(), limit: 10 })
                    .collect();
                let held = if counted { entries.clone() } else { Vec::new() };
                let target = if fail_open { entries } else { held.clone() };
                CallLimiterState::from_parts(
                    "call#k".into(),
                    counted || sent,
                    (held, 1),
                    (target, 1),
                    1,
                    None,
                )
            })
    }

    /// A pre-emitted rule release (the dedupe case).
    fn arb_pre_release() -> impl Strategy<Value = SoftBoundedEffect> {
        Just(SoftBoundedEffect::ReleaseLimiter { key: "call#k".into() })
    }

    proptest! {
        #[test]
        fn extracted_enforce_is_equivalent_to_the_old_body(
            before_state in arb_state(),
            after_state in arb_state(),
            limiter in arb_limiter_state(),
            pre_releases in proptest::collection::vec(arb_pre_release(), 0..3),
            pre_write_cdr in proptest::bool::ANY,
            pre_cancel_all in proptest::bool::ANY,
            pre_remove_call in proptest::bool::ANY,
            timer_count in 0..3usize,
        ) {
            let mut before = test_call();
            before.state = before_state;

            let mut after = test_call();
            after.state = after_state;
            if after_state != CallModelState::Active {
                // A synthetic terminal states its cause as every live path does.
                after = call::helpers::record_termination(
                    after,
                    0,
                    TerminationCause::Supervisor,
                    None,
                );
            }
            after.limiter = limiter;
            after.timers = (0..timer_count)
                .map(|i| TimerEntry {
                    key: TimerKey::call(TimerType::Keepalive),
                    generation: 1,
                    fire_at: 1_000 + i as i64,
                })
                .collect();

            let mut effects = HandlerEffects::new();
            if pre_cancel_all {
                effects.critical.push(CriticalStateEffect::CancelAllTimers);
            }
            if pre_remove_call {
                effects.critical.push(CriticalStateEffect::RemoveCall);
            }
            effects.critical.push(CriticalStateEffect::CancelTimer {
                key: TimerKey::leg(TimerType::NoAnswer, "b-1"),
            });
            effects.soft.extend(pre_releases);
            if pre_write_cdr {
                effects.buffered.push(BufferedObservabilityEffect::WriteCdr);
            }

            let result = HandlerResult { call: after, effects };

            let reference = reference_enforce(&before, result.clone());
            // `answer_unanswered_a_leg = false`: the reference states the limiter
            // and CDR obligations only; the ADR-0022 unanswered-a-leg final is
            // additive and covered by its own tests above.
            let actual = invariants::enforce(
                &ObligationSet::core(),
                &before,
                result,
                0,
                invariants::UnansweredCaller::Leave,
                &mut TurnNotes::default(),
            );

            prop_assert_eq!(
                &reference.call,
                &actual.call,
                "call (incl. cleared timers) must match"
            );
            let lanes = |r: &HandlerResult| {
                format!("{:?}", (&r.effects.critical, &r.effects.soft, &r.effects.buffered))
            };
            prop_assert_eq!(
                lanes(&reference),
                lanes(&actual),
                "effect lanes must be identical (order included)"
            );
        }
    }
}

// ── the limiter settle: one release per call that sent an admit ─────────────
mod limiter_settle {
    use super::*;
    use b2bua_core::effects::{FireAndForgetEffect, HandlerEffects, SoftBoundedEffect};
    use b2bua_core::obligations::ObligationSet;
    use call::CallLimiterState;

    /// The releases the turn sends.
    fn releases(effects: &HandlerEffects) -> usize {
        effects
            .soft
            .iter()
            .filter(|e| matches!(e, SoftBoundedEffect::ReleaseLimiter { .. }))
            .count()
    }

    #[test]
    fn a_counted_call_is_released_once_whatever_its_set() {
        let mut call = test_call();
        call.limiter = CallLimiterState::admitted(
            call.limiter.key().to_string(),
            1,
            vec![
                call::LimiterEntry { id: "x".into(), limit: 10 },
                call::LimiterEntry { id: "x".into(), limit: 10 },
                call::LimiterEntry { id: "y".into(), limit: 10 },
            ],
        );
        let mut effects = HandlerEffects::new();
        ObligationSet::core().settle(&call, &mut effects);
        assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
        assert_eq!(ObligationSet::core().owed(&call).len(), 2, "the limiter and the CDR");

        // Settling again discharges nothing more.
        ObligationSet::core().settle(&call, &mut effects);
        assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
    }

    #[test]
    fn a_rule_emitted_release_discharges_the_call() {
        let mut call = test_call();
        call.limiter = CallLimiterState::admitted(
            call.limiter.key().to_string(),
            1,
            vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
        );
        let mut effects = HandlerEffects::new();
        effects
            .soft
            .push(SoftBoundedEffect::ReleaseLimiter { key: call.limiter.key().to_string() });
        ObligationSet::core().settle(&call, &mut effects);
        assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
    }

    #[test]
    fn a_call_that_sent_no_admit_owes_no_release() {
        let mut call = test_call();
        call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
        let mut effects = HandlerEffects::new();
        ObligationSet::core().settle(&call, &mut effects);
        assert_eq!(releases(&effects), 0, "{:?}", effects.soft);
        assert!(ObligationSet::core().owed(&call).iter().all(|o| o.kind == "cdr"));
    }

    #[test]
    fn an_uncounted_call_that_sent_an_admit_is_released_once() {
        let mut call = test_call();
        call.limiter = unconfirmed(call.limiter.key());
        let mut effects = HandlerEffects::new();
        ObligationSet::core().settle(&call, &mut effects);
        assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
        assert_eq!(ObligationSet::core().owed(&call).len(), 2, "the limiter and the CDR");
        ObligationSet::core().settle(&call, &mut effects);
        assert_eq!(releases(&effects), 1, "{:?}", effects.soft);
    }

    /// Execute `actions` on `call` for a turn, then enforce.
    fn turn(call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
        let event = fold_event(call, "call-failure-result", "failover", route_fold_payload());
        let config = B2buaConfig::default();
        let ctx = ctx_for(call, &event, &config);
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        let result = exec.execute(actions, call, &ctx);
        invariants::enforce(
            &ObligationSet::core(),
            call,
            result,
            0,
            invariants::UnansweredCaller::Leave,
            &mut TurnNotes::default(),
        )
    }

    #[test]
    fn a_call_ended_holding_a_set_or_after_a_dropped_one_releases_once() {
        // One release of the call, which frees whatever set the limiter
        // holds for it; a call that never sent an admit ends with none.
        let end = [RuleAction::TerminateCall { cause: TerminationCause::Supervisor, by_leg: None }];
        let mut call = test_call();
        call.limiter = CallLimiterState::admitted(
            call.limiter.key().to_string(),
            1,
            vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
        );
        let result = turn(&call, &end);
        assert_eq!(result.call.state, CallModelState::Terminated, "the turn ends the call");
        assert_eq!(releases(&result.effects), 1, "{:?}", result.effects.soft);

        call.limiter = unconfirmed(call.limiter.key());
        let result = turn(&call, &end);
        assert_eq!(releases(&result.effects), 1, "{:?}", result.effects.soft);

        call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
        let result = turn(&call, &end);
        assert_eq!(result.call.state, CallModelState::Terminated);
        assert_eq!(releases(&result.effects), 0, "{:?}", result.effects.soft);
    }

    /// A turn that sends an admit off the call's turn (a service's
    /// replacement, a failure consult, a release consult) owes the call's
    /// release from that turn on: the admit may land while the call is taken
    /// over or ended before its report comes back.
    #[test]
    fn a_turn_sending_an_admit_owes_the_release() {
        for action in [
            RuleAction::ReplaceAdmissionSet {
                correlation_id: "svc:1".into(),
                entries: vec![call::LimiterEntry { id: "x".into(), limit: 1 }],
                moves_call: false,
            },
            RuleAction::FailureAsyncHttp { request: serde_json::json!({}) },
            RuleAction::ReleaseAsyncHttp { request: serde_json::json!({}) },
        ] {
            let mut call = test_call();
            call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
            let result = turn(&call, std::slice::from_ref(&action));
            assert!(result.call.limiter.owed_release().is_some(), "{action:?} owes the release");
        }
    }

    /// A service's replacement is the call's target from the turn that sends
    /// it: a change computed before its report comes back sees it.
    #[test]
    fn a_service_replacement_is_the_target_from_its_sending_turn() {
        let mut call = test_call();
        call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
        let entries = vec![call::LimiterEntry { id: "x".into(), limit: 1 }];
        let action = RuleAction::ReplaceAdmissionSet {
            correlation_id: "svc:1".into(),
            entries: entries.clone(),
            moves_call: false,
        };
        let result = turn(&call, &[action]);
        assert_eq!(result.call.limiter.target(), entries);
    }

    /// A service moves a call that holds nothing and owes nothing onto an
    /// empty set: nothing can have landed and nothing is asked, so no admit
    /// leaves; the move is recorded on the turn, and the result re-enters as
    /// `not_sent`.
    #[test]
    fn a_move_onto_nothing_with_nothing_to_send_sends_no_admit() {
        let mut call = test_call();
        let moved_onto = vec![call::LimiterEntry { id: "y".into(), limit: 1 }];
        call.limiter = CallLimiterState::from_parts(
            call.limiter.key().to_string(),
            false,
            (Vec::new(), 0),
            (Vec::new(), 0),
            0,
            Some(moved_onto),
        );
        assert!(call.limiter.runs_uncounted(), "the call runs on y, which nothing holds");
        let action = RuleAction::ReplaceAdmissionSet {
            correlation_id: "svc:1".into(),
            entries: Vec::new(),
            moves_call: true,
        };
        let result = turn(&call, &[action]);
        assert_eq!(result.call.limiter.runs_on(), Some(&[][..]), "moved onto nothing");
        assert!(
            !result.call.limiter.runs_uncounted() && result.call.limiter.owed_release().is_none()
        );
        let ff = &result.effects.fire_and_forget;
        assert!(
            !ff.iter().any(|e| matches!(e, FireAndForgetEffect::LimiterAdmit { .. })),
            "no admit leaves: {ff:?}"
        );
        assert!(
            ff.iter().any(|e| matches!(e, FireAndForgetEffect::Reenter(ev)
                if matches!(ev.as_ref(), CallEvent::InternalEvent { topic, outcome, .. }
                    if topic == "limiter-admit-result" && outcome == "not_sent"))),
            "the result re-enters as not_sent: {ff:?}"
        );
    }

    /// The state of a call whose admit a cap refused, dropping its set: it
    /// holds nothing and owes its release.
    fn unconfirmed(key: &str) -> CallLimiterState {
        let mut state = CallLimiterState::uncounted(key.to_string());
        state.apply_admit(&call::AdmitReport {
            key: key.to_string(),
            change: 1,
            entries: vec![call::LimiterEntry { id: "x".into(), limit: 1 }],
            outcome: call::AdmitOutcome::Rejected {
                limiter_id: "x".into(),
                held: call::LimiterHeld { change: 1, entries: vec![] },
            },
        });
        state
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
        RuleAction::arm(TimerKey::call(t), TimerDelay::secs(secs))
    }

    #[test]
    fn distinct_keys_coexist_and_same_key_reschedule_supersedes() {
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        let call = test_call();
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 1_000 };
        let event = info_like_event(&call);
        let ctx = ctx_at(&call, &event, &config);

        let fast = TimerType::service(SVC, "fast");
        let slow = TimerType::service(SVC, "slow");
        let result =
            exec.execute(&[schedule(fast.clone(), 3), schedule(slow.clone(), 6)], &call, &ctx);
        let ids: Vec<String> = result.call.timers.iter().map(|t| format!("{:?}", t.key)).collect();
        assert_eq!(
            ids,
            vec!["Service:svc-a:fast on call", "Service:svc-a:slow on call"],
            "two keys → two distinct ledger entries",
        );

        // Re-arm the SAME key: replaced in place, not appended.
        let ctx2 = ctx_at(&result.call, &event, &config);
        let rearmed = exec.execute(&[schedule(fast.clone(), 10)], &result.call, &ctx2);
        assert_eq!(rearmed.call.timers.len(), 2, "same-key re-arm supersedes");
        let entry = rearmed
            .call
            .timers
            .iter()
            .find(|t| *t.timer_type() == fast)
            .expect("fast entry present");
        assert_eq!(entry.fire_at, 1_000 + 10_000, "the re-arm's deadline won");
        assert_eq!(*entry.timer_type(), fast);

        // A cancel of the key removes exactly its own entry.
        let ctx3 = ctx_at(&rearmed.call, &event, &config);
        let cancelled =
            exec.execute(&[RuleAction::cancel(TimerKey::call(fast))], &rearmed.call, &ctx3);
        let ids: Vec<String> =
            cancelled.call.timers.iter().map(|t| format!("{:?}", t.key)).collect();
        assert_eq!(ids, vec!["Service:svc-a:slow on call"], "cancel hits only its own key");
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
            key: TimerKey::call(mine.clone()),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
        };
        let core_fire = CallEvent::Timer {
            key: TimerKey::call(TimerType::GlobalDuration),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
        };
        let foreign_fire = CallEvent::Timer {
            key: TimerKey::call(TimerType::service(MachineId::new("svc-b"), "fast")),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
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
            key: TimerKey::call(TimerType::service(SVC, "slow")),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
        };
        let ctx = ctx_at(&call, &other_key_fire, &config);
        assert!(!exact.accepts_columns(&ctx), "exact-key matcher rejects a sibling key");
        assert!(wildcard.accepts_columns(&ctx), "per-service wildcard accepts a sibling key");
    }

    fn info_like_event(call: &call::Call) -> CallEvent {
        CallEvent::Timer {
            key: TimerKey::call(TimerType::service(SVC, "fast")),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
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
            resolved: &call::helpers::UNRESOLVED,
        }
    }
}

mod declared_capabilities;
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
        resolved: &call::helpers::UNRESOLVED,
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
        key: TimerKey::call(TimerType::Keepalive),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, &call, &ctx);
    let keepalive = ranked.iter().find(|r| r.id == "keepalive").expect("keepalive is a candidate");
    let mut actions = (keepalive.handle)(&ctx).expect("keepalive handles its own timer").actions;
    actions
        .push(RuleAction::arm(TimerKey::call(TimerType::GlobalDuration), TimerDelay::secs(3600)));

    let mut id_gen = scope(0x65);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms };
    let result = exec.execute(&actions, &call, &ctx);

    let armed = result
        .call
        .timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::Keepalive)
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
        .find(|t| *t.timer_type() == TimerType::GlobalDuration)
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
            CriticalStateEffect::ScheduleTimer(t) if *t.timer_type() == TimerType::Keepalive => {
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
/// defect at that site, noted on the turn, and the deadline is clamped to one
/// interval out: an early probe costs nothing, a probe a cadence late costs
/// the call its peer's keepalive tolerance, and a foreign-frame deadline is
/// never persisted and replicated.
#[test]
fn arming_a_keepalive_beyond_one_cadence_is_a_defect_and_is_clamped() {
    let now_ms = 1_000_000;
    let config = B2buaConfig::default();
    let call = test_call();
    let event = CallEvent::Timer {
        key: TimerKey::call(TimerType::Keepalive),
        generation: 0,
        call_ref: call.call_ref.clone(),
        incarnation: None,
    };
    let ctx = timer_ctx(&call, &event, &config, now_ms);
    let mut id_gen = scope(0x65);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms };
    let result = exec.execute(
        &[RuleAction::arm(
            TimerKey::call(TimerType::Keepalive),
            TimerDelay::secs(config.keepalive_interval_sec * 2),
        )],
        &call,
        &ctx,
    );
    let armed = result
        .call
        .timers
        .iter()
        .find(|t| *t.timer_type() == TimerType::Keepalive)
        .expect("the probe is armed");
    assert_eq!(
        armed.fire_at,
        now_ms + config.keepalive_interval_sec * 1000,
        "the over-cadence deadline is clamped to one interval out",
    );
    let defects: Vec<_> = id_gen.notes.defects().iter().map(|d| d.name).collect();
    assert_eq!(defects, [b2bua_core::turn_notes::KEEPALIVE_WITHIN_CADENCE]);
}

// ── §13.3.1.4 answered-2xx discharge (the engine's, before the rules) ────────

/// A call whose a-leg dialog retains the initial-INVITE 2xx under the key the
/// caller's ACK carries (To-tag `btag`, CSeq 1), its ladder armed — the state
/// the engine must discharge.
fn call_with_retained_answered_2xx() -> call::Call {
    let mut call = test_call();
    let mut b = b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, b);
    // The confirmed a-leg UAS dialog the answer seam retained the 2xx on.
    let mut answered = a_dialog(&call, "btag", DialogState::Confirmed);
    answered.answered_2xx = Some(call::Unacked2xx {
        dialog_tag: "btag".into(),
        cseq: 1,
        emission: call::RetainedEmission::paced(
            b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
            ("127.0.0.1".into(), 5060),
            sip_retransmit::Class::Final2xx,
            call::Repeated::response("INVITE", 200),
        )
        .0,
    });
    call.a_leg.dialogs.insert(answered);
    call.a_leg.own_tag = Some("btag".into());
    let obligation = answered_obligation();
    for (key, fire_at) in [
        (TimerKey::obligation(TimerType::Rung, obligation.clone()), 500),
        (TimerKey::obligation(TimerType::RepeatGiveUp, obligation), 32_000),
    ] {
        call::helpers::arm_timer(&mut call, key, fire_at);
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
) -> (Option<call::Obligation>, call::Call, b2bua_core::effects::HandlerEffects) {
    let ack = in_dialog_request(Method::Ack);
    let event = CallEvent::Sip {
        message: Box::new(SipMessage::Request(ack)),
        src: "127.0.0.1:5060".parse().unwrap(),
        matched_client_txn: false,
    };
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let mut call = call.clone();
    let mut fx = b2bua_core::effects::HandlerEffects::new();
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
        after.a_leg.dialogs.confirmed().expect("dialog kept").answered_2xx.is_none(),
        "the retained datagram is discharged from the replicated body",
    );
    assert!(
        after.timers.iter().all(|t| t.key.obligation_of().is_none()),
        "both ladder timers leave the ledger, got {:?}",
        after.timers,
    );
    let cancelled: Vec<String> = fx
        .critical
        .iter()
        .filter_map(|e| match e {
            CriticalStateEffect::CancelTimer { key } => Some(format!("{key:?}")),
            _ => None,
        })
        .collect();
    assert_eq!(
        cancelled,
        vec!["Rung on AckOf2xx:a:btag:1", "RepeatGiveUp on AckOf2xx:a:btag:1"],
        "both ladder timers leave the driver under their keys",
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
        after.a_leg.dialogs.confirmed().expect("dialog kept").answered_2xx.is_some(),
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
        resolved: &call::helpers::UNRESOLVED,
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

// ADR-0032 X5 — a ladder give-up's settlement after the rules (`settle_give_up`,
// the router's step after the rule chain on every `RepeatGiveUp`): an un-ACKed
// 2xx ends the session whatever a service rule made of the give-up.
mod ladder_give_up {
    use super::*;
    use b2bua_core::effects::OutboundBody;
    use call::{CdrEventType, Obligation, RetainedEmission, Unacked2xx};
    use sip_retransmit::Class;

    const A_TAG: &str = "a-svc";

    /// An answered, bridged call: the a-leg confirmed under `A_TAG` with its
    /// INITIAL 2xx (CSeq 1) still awaiting the caller's ACK, the b-leg
    /// confirmed and ACKed.
    fn answered_call() -> call::Call {
        let mut call = test_call();
        confirm_a(&mut call);
        let mut answered = a_dialog(&call, A_TAG, DialogState::Confirmed);
        answered.answered_2xx = Some(retained_2xx(A_TAG, 1, 5060));
        call.a_leg.dialogs = vec![answered].into();
        call.a_leg.own_tag = Some(A_TAG.into());
        let mut b = super::b_leg_pending();
        call::testkit::stage_leg(&mut b, LegState::Confirmed);
        with_dialog(&mut b, "bobtag").peer_cseq = Some(1);
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
            key: TimerKey::obligation(TimerType::RepeatGiveUp, obligation.clone()),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
        }
    }

    fn is_2xx_give_up(ctx: &RuleContext) -> bool {
        ctx.timer_type() == Some(&TimerType::RepeatGiveUp)
            && matches!(
                ctx.timer_key().and_then(TimerKey::obligation_of),
                Some(Obligation::AckOf2xx { .. })
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
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        let event = give_up_of(call, obligation);
        let mut call = call.clone();
        let mut fx = b2bua_core::effects::HandlerEffects::new();
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
            resolved: &call::helpers::UNRESOLVED,
        };
        let picked: Vec<&str> = pick_ranked(rules, &call, &ctx).iter().map(|r| r.id).collect();
        assert_eq!(
            picked.first().copied(),
            Some("svc-parks-the-give-up"),
            "the service rule outranks CORE: {picked:?}"
        );
        let ruled = execute_rules(rules, &call, &ctx, &mut exec, &ObligationSet::core());
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
            ruled.call.a_leg.dialogs.confirmed().unwrap().answered_2xx.is_some(),
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
            settled.call.a_leg.dialogs.confirmed().unwrap().answered_2xx.is_none(),
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
        call.a_leg.dialogs.confirmed_mut().unwrap().answered_2xx = None;
        call.b_legs[0].dialogs.confirmed_mut().unwrap().pending_reinvite_2xx =
            Some(retained_2xx("svc", 7, 5070));
        let obligation =
            Obligation::AckOf2xx { leg: "b-1".into(), dialog_tag: "svc".into(), cseq: 7 };
        let mut rules = vec![parking_rule(is_2xx_give_up)];
        rules.extend(default_rules());

        let (_, settled) = give_up_turn(&call, &obligation, &rules);

        assert_eq!(settled.call.state, CallModelState::Terminating);
        assert!(
            settled.call.b_legs[0].dialogs.confirmed().unwrap().pending_reinvite_2xx.is_none(),
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
    use b2bua_core::obligations::ObligationSet;
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
            RuleAction::BeginTermination {
                reason: Some("deadline".into()),
                cause: TerminationCause::Timeout(call::TimeoutKind::Setup),
                by_leg: None,
            },
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
        call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
        // A ringing callee holds a terminating call until it is settled.
        let mut ringing = b_leg_pending();
        call::testkit::stage_leg(&mut ringing, LegState::Early);
        call = call::helpers::add_b_leg(call, ringing);
        call.sm_cursors.insert(SVC, StateLabel::new("Armed"));
        call
    }

    fn deadline_fire(call: &call::Call) -> CallEvent {
        CallEvent::Timer {
            key: TimerKey::call(DEADLINE),
            generation: 0,
            call_ref: call.call_ref.clone(),
            incarnation: None,
        }
    }

    fn run(rules: &[RuleDefinition], call: &call::Call, event: &CallEvent) -> HandlerResult {
        let config = B2buaConfig::default();
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        let ctx = ctx_for(call, event, &config);
        execute_rules(rules, call, &ctx, &mut exec, &ObligationSet::core())
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
        // Entering `terminating` retires the call's scope: every service
        // watchdog, every per-leg NoAnswer entry and the call-level cap leave
        // the ledger and the driver, and every service machine's cursor goes;
        // the teardown's own safety net and the lifecycle projection stay.
        let mut call = armed_call(CallModelState::Active);
        let no_answer = TimerKey::leg(TimerType::NoAnswer, "b-1");
        let deadline = TimerKey::call(DEADLINE);
        let duration = TimerKey::call(TimerType::GlobalDuration);
        for key in [no_answer.clone(), deadline.clone(), duration.clone()] {
            call::helpers::arm_timer(&mut call, key, 5_000);
        }
        call.sm_cursors
            .insert(b2bua_core::rules::invariants::GLOBAL_CALL_MACHINE, StateLabel::new("Active"));
        let event = CallEvent::Cancelled {
            call_id: call.a_leg.call_id.clone(),
            from_tag: call.a_leg_invite.from_tag.clone(),
            invite_cseq: None,
            in_dialog: false,
            headers: vec![],
            to_tag: None,
        };
        let config = B2buaConfig::default();
        let ctx = ctx_for(&call, &event, &config);
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        let result = exec.execute(
            &[RuleAction::BeginTermination {
                reason: Some("CANCEL".into()),
                cause: TerminationCause::RemoteCancel,
                by_leg: Some("a".into()),
            }],
            &call,
            &ctx,
        );
        let ledger: Vec<&TimerType> = result.call.timers.iter().map(|t| t.timer_type()).collect();
        assert_eq!(
            ledger,
            vec![&TimerType::TerminatingTimeout],
            "the call's timers left the ledger; the teardown's safety net is armed",
        );
        for key in [&no_answer, &deadline, &duration] {
            assert!(
                result.effects.critical.iter().any(|e| matches!(
                    e,
                    CriticalStateEffect::CancelTimer { key: cancelled } if cancelled == key
                )),
                "the live fiber {key:?} is cancelled, got {:?}",
                result.effects.critical,
            );
        }
        let machines: Vec<&str> = result.call.sm_cursors.keys().map(|m| m.as_str()).collect();
        assert_eq!(machines, vec!["global-call"], "every service machine is deactivated");
    }
}

mod lifecycle_delivery;

// ── the limiter refresh is always armed on a counted live call ───────────────
mod limiter_refresh_armed {
    use super::*;
    use b2bua_core::effects::{CriticalStateEffect, HandlerResult};
    use call::{CallLimiterState, TimerEntry, TimerKey, TimerType};

    /// The turn's refresh arming under a `period_ms` refresh period.
    fn arm(result: HandlerResult, now_ms: i64, period_ms: u64) -> HandlerResult {
        b2bua_core::limiter::call::arm_refresh(
            result,
            now_ms,
            std::time::Duration::from_millis(period_ms),
        )
    }

    fn refresh_timer(fire_at: i64) -> TimerEntry {
        TimerEntry { key: TimerKey::call(TimerType::LimiterRefresh), generation: 1, fire_at }
    }

    fn scheduled(result: &HandlerResult) -> Vec<i64> {
        result
            .effects
            .critical
            .iter()
            .filter_map(|e| match e {
                CriticalStateEffect::ScheduleTimer(t)
                    if *t.timer_type() == TimerType::LimiterRefresh =>
                {
                    Some(t.fire_at)
                }
                _ => None,
            })
            .collect()
    }

    fn counted_active() -> call::Call {
        let mut call = test_call();
        call.state = CallModelState::Active;
        call.limiter = CallLimiterState::admitted(
            call.limiter.key().to_string(),
            1,
            vec![call::LimiterEntry { id: "x".into(), limit: 10 }],
        );
        call
    }

    #[test]
    fn a_dropped_fire_is_re_armed_on_the_next_turn() {
        // The ledger still carries an entry past due — a fire the per-call
        // queue dropped, or a copy's restored one its first turn supersedes:
        // the turn re-arms it due now, on the record too, so the refresh it
        // owes runs at once.
        let mut call = counted_active();
        call.timers = vec![refresh_timer(9_000)];
        let result = arm(HandlerResult::new(call), 10_000, 40_000);
        assert_eq!(scheduled(&result), vec![10_000]);
        let armed: Vec<i64> = result
            .call
            .timers
            .iter()
            .filter(|t| *t.timer_type() == TimerType::LimiterRefresh)
            .map(|t| t.fire_at)
            .collect();
        assert_eq!(armed, vec![10_000], "one entry, replaced");

        // No entry at all: armed as well.
        let result = arm(HandlerResult::new(counted_active()), 10_000, 40_000);
        assert_eq!(scheduled(&result), vec![50_000]);
    }

    #[test]
    fn a_live_refresh_is_left_alone() {
        let mut call = counted_active();
        call.timers = vec![refresh_timer(30_000)];
        let result = arm(HandlerResult::new(call), 10_000, 40_000);
        assert!(scheduled(&result).is_empty(), "{:?}", result.effects.critical);
    }

    #[test]
    fn a_refresh_armed_past_one_period_is_brought_forward() {
        // Armed 40 s ahead, the learnt lease now sets a 2 s period: brought
        // to now + 2 s.
        let mut call = counted_active();
        call.timers = vec![refresh_timer(50_000)];
        let result = arm(HandlerResult::new(call), 10_000, 2_000);
        assert_eq!(scheduled(&result), vec![12_000]);
        let mut call = counted_active();
        call.state = CallModelState::Terminating;
        call.timers = vec![refresh_timer(50_000)];
        let result = arm(HandlerResult::new(call), 10_000, 2_000);
        assert!(scheduled(&result).is_empty(), "an ending call is left alone");
    }

    #[test]
    fn an_uncounted_or_ending_call_arms_nothing() {
        let mut call = counted_active();
        call.limiter = CallLimiterState::uncounted(call.limiter.key().to_string());
        let result = arm(HandlerResult::new(call), 10_000, 40_000);
        assert!(scheduled(&result).is_empty());
        let mut call = counted_active();
        call.state = CallModelState::Terminating;
        let result = arm(HandlerResult::new(call), 10_000, 40_000);
        assert!(scheduled(&result).is_empty());
    }
}

// ── reaper-overflow: the wire teardown of a call flooded past its overflow ──

/// The reaper's `outcome` verdict for `call`.
fn reaper_verdict(call: &call::Call, outcome: &str) -> CallEvent {
    CallEvent::InternalEvent {
        call_ref: call.call_ref.clone(),
        topic: b2bua_core::reaper_verdict::REAPER_TOPIC.into(),
        outcome: outcome.into(),
        payload: serde_json::json!({}),
        body: Vec::new(),
        incarnation: None,
    }
}

/// `call` run through the default rules on the `overflow` verdict.
fn on_overflow_verdict(call: &call::Call) -> HandlerResult {
    on_verdict(call, b2bua_core::reaper_verdict::OUTCOME_OVERFLOW)
}

/// `call` run through the default rules on the reaper's `outcome` verdict.
fn on_verdict(call: &call::Call, outcome: &str) -> HandlerResult {
    let event = reaper_verdict(call, outcome);
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
    let ctx = ctx_for(call, &event, &config);
    execute_rules(&default_rules(), call, &ctx, &mut exec, &ObligationSet::core())
}

/// A call whose b-leg rings on an INVITE its CANCEL can name.
fn ringing_call() -> call::Call {
    let mut leg = b_leg_pending();
    // The INVITE this leg is ringing on, which its CANCEL names (RFC 3261 §9.1).
    let dest = call::HostPort { host: "10.0.0.2".into(), port: 5070 };
    let handle = call::InviteTxnHandle::new("z9hG4bK-b1", 1, invite().image().to_vec(), dest);
    leg.outbound_invite.as_mut().expect("a dialled leg").txn = Some(handle);
    let call = call::helpers::add_b_leg(test_call(), leg);
    let (_, actions) = handle_from_b1(&call, &b_leg_ringing_event(false));
    let ringing = execute_from_b1(&call, &b_leg_ringing_event(false), &actions).call;
    assert_eq!(ringing.b_legs[0].state(), LegState::Early);
    ringing
}

/// A ringing call condemned for flooding its overflow: its callee is alive
/// and is CANCELed, and the CDR names the teardown.
#[test]
fn an_overflow_verdict_on_a_ringing_call_cancels_the_callee() {
    let result = on_overflow_verdict(&ringing_call());
    assert_eq!(requests_to(&result, "b-1", "CANCEL"), 1, "the ringing callee is CANCELed");
    assert!(
        result.call.cdr_events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")),
        "the CDR names the teardown"
    );
}

/// A call already terminating — bob hung up — is left to that teardown: the
/// verdict records nothing, keeps the cause, and re-arms no timer.
#[test]
fn an_overflow_verdict_on_a_terminating_call_changes_nothing() {
    let call = call::helpers::add_b_leg(test_call(), b_leg_pending());
    let mut ending =
        call::helpers::record_termination(call, 0, TerminationCause::RemoteBye, Some("b-1".into()));
    ending.state = CallModelState::Terminating;

    let result = on_overflow_verdict(&ending);
    assert!(
        !result.call.cdr_events.iter().any(|e| e.reason.as_deref() == Some("dispatch-overflow")),
        "no overflow teardown is recorded"
    );
    assert_eq!(
        result.call.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the cause bob gave stays"
    );
    assert!(
        !result.effects.critical.iter().any(|e| matches!(
            e,
            CriticalStateEffect::ScheduleTimer(t)
                if *t.timer_type() == TimerType::TerminatingTimeout
        )),
        "the terminating timer is not re-armed"
    );
}

// ── reaper-message-cap: the work bound ──────────────────────────────────────

/// An Active call past its lifetime message cap is torn down on the wire:
/// the ringing callee is CANCELed, the cause is the cap.
#[test]
fn a_message_cap_verdict_on_a_ringing_call_cancels_the_callee() {
    let result = on_verdict(&ringing_call(), b2bua_core::reaper_verdict::OUTCOME_MESSAGE_CAP);
    assert_eq!(requests_to(&result, "b-1", "CANCEL"), 1, "the ringing callee is CANCELed");
    assert_eq!(
        result.call.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::MessageCap)
    );
    assert!(result
        .call
        .cdr_events
        .iter()
        .any(|e| e.reason.as_deref() == Some("message-cap-lifetime")));
}

/// A terminating call past its cap — its peers already sent their BYE — is
/// forced terminal now: its unresolved legs resolved, the reason recorded,
/// the cause it already had kept.
#[test]
fn a_message_cap_verdict_on_a_terminating_call_forces_it_terminal() {
    let mut ending = call::helpers::record_termination(
        ringing_call(),
        0,
        TerminationCause::RemoteBye,
        Some("b-1".into()),
    );
    ending.state = CallModelState::Terminating;
    let result = on_verdict(&ending, b2bua_core::reaper_verdict::OUTCOME_MESSAGE_CAP);
    assert_eq!(requests_to(&result, "b-1", "CANCEL"), 0, "nothing more on the wire");
    assert!(result.call.b_legs.iter().all(call::helpers::leg_is_resolved));
    assert!(result
        .call
        .cdr_events
        .iter()
        .any(|e| e.reason.as_deref() == Some("message-cap-lifetime")));
    assert_eq!(
        result.call.termination.as_ref().map(|t| t.cause),
        Some(TerminationCause::RemoteBye),
        "the first termination names who ended the call"
    );
}

// A re-INVITE's body names its author: the peer whose session the dialog
// carries continues it as written, the same bytes from this stack are restated
// under that session (RFC 3264 §8).
mod sdp_body_author {
    use super::*;
    use b2bua_core::effects::OutboundBody;

    const BOB_V5: &str =
        "v=0\r\no=bob 202 5 IN IP4 192.0.2.2\r\ns=-\r\nc=IN IP4 192.0.2.2\r\nt=0 0\r\nm=audio 20002 RTP/AVP 0\r\n";

    fn reinvite_body(author: BodyAuthor) -> String {
        let config = B2buaConfig::default();
        let mut call = call_with_retained_answered_2xx();
        call.a_leg.sdp_session = call::LegSdpSession {
            sent_origin: Some("bob 202 1 IN IP4 192.0.2.2".into()),
            sent_media: vec!["audio 20000 RTP/AVP 0".into()],
            session_author: Some("b-1".into()),
            session_id: Some("202".into()),
            ..Default::default()
        };
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(invite())),
            src: "127.0.0.1:5060".parse().unwrap(),
            matched_client_txn: false,
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
            resolved: &call::helpers::UNRESOLVED,
        };
        let mut id_gen = scope(1);
        let mut exec = ActionExecutor { config: &config, turn: &mut id_gen, now_ms: 0 };
        let reinvite = RuleAction::SendReinvite {
            leg_id: "a".into(),
            body: Some(Body::new(BOB_V5.as_bytes().to_vec(), None, author)),
            add_headers: vec![],
        };
        let result = exec.execute(&[reinvite], &call, &ctx);
        let effect = result.effects.outbound.first().expect("the re-INVITE leaves");
        let OutboundBody::Request(req) = &effect.body else { panic!("a request") };
        String::from_utf8_lossy(req.body()).into_owned()
    }

    #[test]
    fn the_sessions_own_author_is_sent_as_written_and_the_stack_is_restated() {
        assert_eq!(reinvite_body(BodyAuthor::Leg("b-1".into())), BOB_V5);
        assert!(
            reinvite_body(BodyAuthor::Stack).contains("o=bob 202 2 IN IP4 192.0.2.2\r\n"),
            "the stack's description continues the session one version up"
        );
    }
}

/// The caller's early dialog `a1` (her INVITE offered) maps to b-2 after a
/// reroute; b-1's reliable 183 answered her offer there. Her UPDATE offer on
/// `a1` is legal on her face, but the stack's offer to b-2 stays open until b-2
/// itself answers reliably (RFC 3264 §4): 491, then nothing to refuse.
#[test]
fn an_update_offer_answered_on_the_caller_face_waits_on_the_current_peer() {
    let mut call = call_with_early_a_dialog("a1");
    call::testkit::stage_leg(&mut call.a_leg, LegState::Early);
    call.a_leg_invite
        .headers
        .push(call::SipHeader { name: "Content-Type".into(), value: "application/sdp".into() });
    let mut carol = b_leg_pending();
    call::testkit::stage_leg(&mut carol, LegState::Terminated);
    with_dialog(&mut carol, "c1");
    let mut bob = b_leg_pending();
    bob.leg_id = "b-2".into();
    call::testkit::stage_leg(&mut bob, LegState::Early);
    with_dialog(&mut bob, "b2");
    call = call::helpers::add_b_leg(call, carol);
    call = call::helpers::add_b_leg(call, bob);
    call.a_leg.dialogs.by_tags_mut("a1", &call.a_leg_invite.from_tag.clone()).unwrap().fronts =
        vec![PeerDialog { leg_id: "b-2".into(), peer_tag: "b2".into() }];
    let shown = |b_leg_id: &str, b_tag: &str| {
        let mut p = call::testkit::shown_provisional(
            b_leg_id,
            b_tag,
            1,
            1,
            "a1",
            1,
            call::Ladder::Acknowledged,
            call::ResponderAck::RelayedPrack,
        );
        p.responder_sdp = true;
        if let Some(s) = p.shown.as_mut() {
            s.carried_sdp = true;
        }
        p
    };
    call.provisionals.push(shown("b-1", "c1"));

    let raw = "UPDATE sip:b2bua@127.0.0.1:5080 SIP/2.0\r\n\
               Via: SIP/2.0/UDP 127.0.0.1:5060;branch=z9hG4bKupd\r\n\
               From: <sip:alice@host>;tag=atag\r\nTo: <sip:bob@host>;tag=a1\r\n\
               Call-ID: c1@alice\r\nCSeq: 2 UPDATE\r\nMax-Forwards: 70\r\n\
               Content-Type: application/sdp\r\nContent-Length: 5\r\n\r\nv=0\r\n";
    let SipMessage::Request(req) = CustomParser::new().parse(raw.as_bytes()).unwrap() else {
        panic!("a request")
    };
    // The gate resolves her UPDATE to her dialog `a1`.
    let resolved = call::helpers::Resolved {
        dialog: Some(call::DialogId { our_tag: "a1".into(), peer_tag: "atag".into() }),
        relay: None,
    };
    let config = B2buaConfig::default();
    let refusal = |call: &call::Call| {
        let event = CallEvent::Sip {
            message: Box::new(SipMessage::Request(req.clone())),
            src: "127.0.0.1:5060".parse().unwrap(),
            matched_client_txn: false,
        };
        let ctx = RuleContext {
            call: RuleCall::new(call),
            call_ref: &call.call_ref,
            event: &event,
            source_leg_id: "a",
            direction: Direction::FromA,
            now_ms: 0,
            config: &config,
            discharged: None,
            resolved: &resolved,
        };
        ctx.update_refusal()
    };
    assert_eq!(refusal(&call), Some(b2bua_sdk::model::GlareRefusal::Crossing));
    call.provisionals.push(shown("b-2", "b2"));
    assert_eq!(refusal(&call), None, "b-2 answered reliably: the offer relays");
}

mod charging_in_dialog;

mod body_descriptors;
mod stated_text_skips;
