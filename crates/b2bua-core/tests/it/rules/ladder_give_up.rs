// ADR-0032 X5 — a ladder give-up's settlement after the rules (`settle_give_up`,
// the router's step after the rule chain on every `RepeatGiveUp`): an un-ACKed
// 2xx ends the session whatever a service rule made of the give-up.
use super::fixtures::*;
use super::*;
use b2bua_core::effects::OutboundBody;
use call::{CdrEventType, Obligation};

const A_TAG: &str = "a-svc";

/// An answered, bridged call: the a-leg confirmed under `A_TAG` with its
/// INITIAL 2xx (CSeq 1) still awaiting the caller's ACK, the b-leg
/// confirmed and ACKed.
fn answered_call() -> call::Call {
    let mut call = test_call();
    confirm_a(&mut call);
    let mut answered = a_dialog(&call, A_TAG, DialogState::Confirmed);
    call::helpers::hold_answered_2xx(&mut answered, 1, retained_2xx(A_TAG, 1, 5060));
    call.a_leg.dialogs = vec![answered].into();
    call.a_leg.own_tag = Some(A_TAG.into());
    let mut b = super::fixtures::b_leg_pending();
    call::testkit::stage_leg(&mut b, LegState::Confirmed);
    with_dialog(&mut b, "bobtag").peer_cseq = Some(1);
    call::helpers::add_b_leg(call, b)
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
    let mut exec = executor(&config, &mut id_gen);
    let event = give_up_of(call, obligation);
    let mut call = call.clone();
    let mut fx = b2bua_core::effects::HandlerEffects::new();
    exec.give_up(&mut call, &mut fx, obligation);
    let ctx = context(&call, &event, "a", &config);
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
    let obligation =
        Obligation::AckOf2xx { leg: call.a_leg.leg_id.clone(), dialog_tag: A_TAG.into(), cseq: 1 };
    let mut rules = vec![parking_rule(is_2xx_give_up)];
    rules.extend(default_rules());

    let (ruled, settled) = give_up_turn(&call, &obligation, &rules);

    // What the rules alone left: the defect's shape.
    assert_eq!(ruled.call.state, CallModelState::Active, "the service parked the call");
    assert!(
        ruled.call.a_leg.dialogs.confirmed().unwrap().answered_invite.is_some(),
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
        settled.call.a_leg.dialogs.confirmed().unwrap().answered_invite.is_none(),
        "no retained datagram survives the give-up"
    );
    let mut byes = byes_in(&settled);
    byes.sort_unstable();
    assert_eq!(byes, vec!["a", "b-1"], "the a-leg dialog and the b-leg it bridges are both BYEd");
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
    call.a_leg.dialogs.confirmed_mut().unwrap().answered_invite = None;
    let callee = call.b_legs[0].dialogs.confirmed_mut().unwrap();
    call::helpers::hold_answered_2xx(callee, 7, retained_2xx("svc", 7, 5070));
    let obligation = Obligation::AckOf2xx { leg: "b-1".into(), dialog_tag: "svc".into(), cseq: 7 };
    let mut rules = vec![parking_rule(is_2xx_give_up)];
    rules.extend(default_rules());

    let (_, settled) = give_up_turn(&call, &obligation, &rules);

    assert_eq!(settled.call.state, CallModelState::Terminating);
    assert!(
        settled.call.b_legs[0].dialogs.confirmed().unwrap().answered_invite.is_none(),
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
