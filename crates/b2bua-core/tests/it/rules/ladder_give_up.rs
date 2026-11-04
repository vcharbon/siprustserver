//! ADR-0032 X5 — a ladder give-up's settlement after the rules
//! (`settle_give_up`, the turn's step after the rule chain on every
//! `RepeatGiveUp`): an un-ACKed 2xx ends the session whatever a service rule
//! made of the give-up.
use b2bua_core::rules::{Match, RuleContext, RuleDefinition, RuleHandleResult, SERVICE_LAYER};
use b2bua_core::turn_driver::{Driver, Step, Turn};
use call::{CallModelState, CdrEventType, Obligation, TimerKey, TimerType};
use sip_message::Method;

use crate::calls::{bridged, ringing};

fn is_2xx_give_up(ctx: &RuleContext) -> bool {
    ctx.timer_type() == Some(&TimerType::RepeatGiveUp)
        && matches!(
            ctx.timer_key().and_then(TimerKey::obligation_of),
            Some(Obligation::AckOf2xx { .. })
        )
}

/// A service rule that answers the give-up and deliberately declines to
/// end anything — it "parks" the call. Ranked above every CORE rule.
fn parking_rule() -> RuleDefinition {
    RuleDefinition::core(
        "svc-parks-the-give-up",
        SERVICE_LAYER,
        &[],
        Match::timer().call_state(CallModelState::Active).filter(is_2xx_give_up),
        |_| Some(RuleHandleResult::new(vec![])),
    )
}

/// The legs `turn` sends a BYE on.
fn byes_in(turn: &Turn) -> Vec<&str> {
    let mut legs: Vec<&str> =
        ["a", "b-1"].into_iter().filter(|leg| turn.request_to(leg, "BYE").is_some()).collect();
    legs.sort_unstable();
    legs
}

/// The reason of the BYE CDR event of `leg` on `call`.
fn bye_marker(call: &call::Call, leg: &str) -> Option<String> {
    call.cdr_events
        .iter()
        .find(|e| e.event_type == CdrEventType::Bye && e.leg_id == leg)
        .and_then(|e| e.reason.clone())
}

/// bob's 2xx relayed to alice, who never ACKs it: its ladder gives up.
#[test]
fn a_service_that_parks_the_unacked_2xx_give_up_does_not_keep_the_session() {
    let driver = Driver::new().with_rule(parking_rule());
    let mut call = ringing(&driver);
    call.run([Step::Answers("b-1", 200)]);
    let turn = call.turned(Step::Fires(TimerType::RepeatGiveUp));
    assert_eq!(turn.rule, Some("svc-parks-the-give-up"), "the service rule outranks CORE");
    assert_eq!(byes_in(turn), ["a", "b-1"], "the a-leg dialog and the b-leg it bridges");

    // The settlement: the session ends with the CORE verdict, and nothing
    // rides the replicated body any more.
    let record = call.call();
    assert_eq!(record.state, CallModelState::Terminating, "RFC 3261 §13.3.1.4: the session ends");
    let caller = record.a_leg.dialogs.iter().find(|d| d.answered_invite.is_some());
    assert!(caller.is_none(), "no retained datagram survives the give-up");
    assert_eq!(bye_marker(record, "a").as_deref(), Some("ack_timeout"), "the CORE rule's marker");
}

/// bob's re-INVITE relayed to alice and her 2xx relayed back, which bob never
/// ACKs: that 2xx's ladder gives up.
#[test]
fn a_parked_reinvite_2xx_give_up_ends_the_session_under_its_own_marker() {
    let driver = Driver::new().with_rule(parking_rule());
    let mut call = bridged(&driver);
    call.run([Step::Sends("b-1", Method::Invite), Step::AnswersSent("a", 200)]);
    let turn = call.turned(Step::Fires(TimerType::RepeatGiveUp));
    assert_eq!(turn.rule, Some("svc-parks-the-give-up"));
    let record = call.call();
    assert_eq!(record.state, CallModelState::Terminating);
    let leg = call::helpers::find_leg(record, "b-1").expect("bob's leg");
    let callee = leg.dialogs.iter().find(|d| d.answered_invite.is_some());
    assert!(callee.is_none(), "the re-INVITE 2xx is not retained either");
    assert_eq!(
        bye_marker(record, "b-1").as_deref(),
        Some("reinvite_ack_timeout"),
        "the CDR names the leg that owed the ACK"
    );
}
