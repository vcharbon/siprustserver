//! The hand-built records and helpers the selection, action-primitive and
//! funnel tests (classes (b)–(d)) read; the outcome tests reach their
//! records through the turn driver.

use super::*;

/// The URI a fixture names as text.
pub(super) fn uri_of(text: &str) -> Uri {
    Uri::parse(&SipStr::owned(text)).expect("readable URI")
}

pub(super) fn invite() -> SipRequest {
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
pub(super) fn in_dialog_request(method: sip_message::Method) -> sip_message::SipRequest {
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
pub(super) fn scope(turn_seq: u64) -> TurnScope {
    TurnScope::of(TurnIds::fixed(turn_seq))
}

pub(super) fn test_call() -> call::Call {
    let src: SocketAddr = "127.0.0.1:5060".parse().unwrap();
    build_initial_call(&invite(), src, &B2buaConfig::default(), &mut scope(1).ids, 0)
}

/// A call whose a-leg already carries the early dialog `a1` (the tag a relayed
/// or minted provisional pinned), the leg itself still Trying.
pub(super) fn call_with_early_a_dialog(a1: &str) -> call::Call {
    let mut call = test_call();
    call.a_leg.dialogs = vec![a_dialog(&call, a1, DialogState::Early)].into();
    call.a_leg.own_tag = Some(a1.into());
    call
}

/// Confirm the caller's leg: her early dialog, else a dialog of hers under
/// `atag`.
pub(super) fn confirm_a(call: &mut call::Call) {
    if !call.a_leg.dialogs.iter().any(|d| d.state == DialogState::Early) {
        let d = a_dialog(call, "atag", DialogState::Confirmed);
        call.a_leg.dialogs.insert(d);
        call.a_leg.own_tag.get_or_insert_with(|| "atag".into());
    }
    call::testkit::stage_leg(&mut call.a_leg, LegState::Confirmed);
}

/// A dialog of the caller's leg under `tag`, in `state`, both sequences at 1.
pub(super) fn a_dialog(call: &call::Call, tag: &str, state: DialogState) -> Dialog {
    let mut d = Dialog::new(tag, call.a_leg_invite.from_tag.clone(), state, 1);
    d.peer_cseq = Some(1);
    d.remote_target = "sip:alice@127.0.0.1:5060".into();
    d
}

/// Give `leg` — dialled under `svc` — the dialog the peer tag `peer_tag`
/// names, confirmed on a confirmed leg, early otherwise, in place of a dialog
/// [`call::testkit::stage_leg`] minted; return it.
pub(super) fn with_dialog<'a>(leg: &'a mut Leg, peer_tag: &str) -> &'a mut Dialog {
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

/// One executor turn, one Cancelled-shaped context, over `call`.
pub(super) fn execute_on(call: &call::Call, actions: &[RuleAction]) -> HandlerResult {
    execute_with(&B2buaConfig::default(), call, actions)
}

/// [`execute_on`] under `config`.
pub(super) fn execute_with(
    config: &B2buaConfig,
    call: &call::Call,
    actions: &[RuleAction],
) -> HandlerResult {
    let event = CallEvent::Cancelled {
        call_id: call.a_leg.call_id.clone(),
        from_tag: call.a_leg_invite.from_tag.clone(),
        invite_cseq: None,
        in_dialog: false,
        headers: vec![],
        to_tag: None,
    };
    let ctx = context(call, &event, "a", config);
    let mut id_gen = scope(1);
    let mut exec = executor(config, &mut id_gen);
    exec.execute(actions, call, &ctx)
}

pub(super) fn finals_to_a(result: &HandlerResult) -> Vec<u16> {
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

/// The `(status, carried)` pairs of the provisionals `result` refused after
/// the a-leg's final.
pub(super) fn late_provisional_refusals(result: &HandlerResult) -> Vec<(u16, u16)> {
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
pub(super) fn provisionals_to_a(result: &HandlerResult) -> Vec<u16> {
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

/// The callee's Contact on its provisional.
pub(super) const CALLEE_RINGING_CONTACT: &str = "sip:bob@10.0.0.2:5070;transport=udp";

/// The proxy that recorded itself on the provisional (RFC 3261 §12.1.2).
pub(super) const CALLEE_RINGING_ROUTE: &str = "sip:10.0.0.7:5060;lr";

/// A `180 Ringing` to the b-leg's initial INVITE, from callee tag `bobtag`,
/// with the callee's Contact and a recorded route; `reliable` states
/// `Require: 100rel` + `RSeq: 1`.
pub(super) fn b_leg_ringing_event(reliable: bool) -> CallEvent {
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
    CallEvent::sip(SipMessage::Response(resp), "10.0.0.2:5070".parse().unwrap())
}

/// The actions the first ranked default rule produces for `event` from `leg`.
pub(super) fn handle_from(
    call: &call::Call,
    event: &CallEvent,
    leg: &str,
) -> (&'static str, Vec<RuleAction>) {
    let config = B2buaConfig::default();
    let ctx = context(call, event, leg, &config);
    let rules = default_rules();
    let ranked = pick_ranked(&rules, call, &ctx);
    let rule = ranked.first().expect("a rule claims the provisional");
    let out = (rule.handle)(&ctx).expect("the rule handles it");
    (rule.id, out.actions)
}

/// The actions the first ranked default rule produces for `event` from `b-1`.
pub(super) fn handle_from_b1(
    call: &call::Call,
    event: &CallEvent,
) -> (&'static str, Vec<RuleAction>) {
    handle_from(call, event, "b-1")
}

/// One executor turn over `call` for `actions`, in the context of `event`
/// arriving from `b-1`.
pub(super) fn execute_from_b1(
    call: &call::Call,
    event: &CallEvent,
    actions: &[RuleAction],
) -> HandlerResult {
    let config = B2buaConfig::default();
    let ctx = context(call, event, "b-1", &config);
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    exec.execute(actions, call, &ctx)
}

pub(super) const TEST_MACHINE: &str = "test-machine";

pub(super) static SM_ACTIVE_S0: [StateLabel; 1] = [StateLabel::new("S0")];

pub(super) static SM_TRANSITIONS: [(StateLabel, StateLabel); 1] =
    [(StateLabel::new("S0"), StateLabel::new("S1"))];

pub(super) fn handle_to_s1(_: &RuleContext) -> Option<RuleHandleResult> {
    Some(RuleHandleResult::new(vec![RuleAction::SetState {
        machine: MachineId::new(TEST_MACHINE),
        to: StateLabel::new("S1"),
    }]))
}

pub(super) fn sm_rule(handle: fn(&RuleContext) -> Option<RuleHandleResult>) -> RuleDefinition {
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

pub(super) fn info_event() -> CallEvent {
    let info = in_dialog_request(sip_message::Method::Info);
    CallEvent::sip(SipMessage::Request(info), "127.0.0.1:5060".parse().unwrap())
}

pub(super) fn ctx_for<'a>(
    call: &'a call::Call,
    event: &'a CallEvent,
    config: &'a B2buaConfig,
) -> RuleContext<'a> {
    context(call, event, "a", config)
}

/// A pending (Trying) b-leg dialled under `svc`, no response yet so no
/// dialog — the state `ConfirmDialog` mutates when the 2xx arrives.
pub(super) fn b_leg_pending() -> Leg {
    Leg {
        leg_id: "b-1".into(),
        call_id: "bcid@x".into(),
        source: RemoteInfo { address: "10.0.0.2".into(), port: 5070 },
        dialogs: Default::default(),
        local_uri: Some("sip:svc@10.0.0.9".into()),
        remote_uri: Some("sip:bob@10.0.0.2".into()),
        contact: "sip:b2bua@127.0.0.1:5060".into(),
        invite_request_uri: Some("sip:bob@10.0.0.2:5070".into()),
        invite: Some(call::RequestRecord::dialled(1, None, OutboundInvite::new("svc", 0))),
        own_tag: None,
        ext: None,
        kind: Some(LegKind::Destination),
        adopted: None,
        attempt: None,
        ring: call::Ring::Silent,
        in_session_timer: None,
        messages: Default::default(),
        sdp_session: Default::default(),
    }
}

/// A 200 to the INVITE numbered `cseq`, retained on `dialog_tag`'s face
/// toward port `port` on the §13.3.1.4 ladder (RFC 3261).
pub(super) fn retained_2xx(dialog_tag: &str, cseq: i64, port: u16) -> call::Unacked2xx {
    call::Unacked2xx {
        dialog_tag: dialog_tag.into(),
        cseq,
        emission: call::RetainedEmission::paced(
            b"SIP/2.0 200 OK\r\n\r\n".to_vec(),
            ("127.0.0.1".into(), port),
            sip_retransmit::Class::Final2xx,
            call::Repeated::response("INVITE", 200),
        )
        .0,
    }
}

/// The engine's discharge step for an in-dialog ACK arriving on
/// `source_leg_id`: what it retired, the call it leaves, and its effects.
pub(super) fn discharge_ack(
    call: &call::Call,
    source_leg_id: &str,
) -> (Option<call::Obligation>, call::Call, b2bua_core::effects::HandlerEffects) {
    let ack = in_dialog_request(Method::Ack);
    let event = CallEvent::sip(SipMessage::Request(ack), "127.0.0.1:5060".parse().unwrap());
    let config = B2buaConfig::default();
    let mut id_gen = scope(1);
    let mut exec = executor(&config, &mut id_gen);
    let mut call = call.clone();
    let mut fx = b2bua_core::effects::HandlerEffects::new();
    let discharged = exec.discharge(&mut call, &mut fx, &event, source_leg_id);
    (discharged, call, fx)
}

/// A route-shaped fold payload (what `callouts::route_result_payload` emits).
pub(super) fn route_fold_payload() -> serde_json::Value {
    serde_json::json!({
        "destination": { "host": "127.0.0.1", "port": 5070 },
        "failed_leg_id": "b-1",
    })
}

/// An internal event of `topic`/`outcome` carrying `payload` on `call`.
pub(super) fn fold_event(
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

/// Put every dialog of `leg` in `state`.
pub(super) fn end_dialogs(leg: &mut Leg, state: DialogState) {
    for d in leg.dialogs.iter_mut() {
        d.state = state.clone();
    }
}

/// Record this stack's CANCEL of `leg`'s INVITE: the leg reads `Cancelling`.
pub(super) fn cancel_sent(leg: &mut Leg) {
    leg.outbound_invite_mut().expect("a dialled leg").cancel_sent = true;
}

/// A call whose a-leg dialog retains the initial-INVITE 2xx under the key the
/// caller's ACK carries (To-tag `btag`, CSeq 1), its ladder armed — the state
/// the engine must discharge.
pub(super) fn call_with_retained_answered_2xx() -> call::Call {
    let mut call = test_call();
    let mut b = b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    call = call::helpers::add_b_leg(call, b);
    // The confirmed a-leg UAS dialog the answer seam retained the 2xx on.
    let mut answered = a_dialog(&call, "btag", DialogState::Confirmed);
    call::helpers::hold_answered_2xx(&mut answered, 1, retained_2xx("btag", 1, 5060));
    call.a_leg.dialogs.insert(answered);
    call.a_leg.own_tag = Some("btag".into());
    *call.a_leg.invite_final_sent_mut() = Some(200);
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
pub(super) fn answered_obligation() -> call::Obligation {
    call::Obligation::AckOf2xx { leg: "a".into(), dialog_tag: "btag".into(), cseq: 1 }
}
