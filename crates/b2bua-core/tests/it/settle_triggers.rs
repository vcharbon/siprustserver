//! The one settle entry at each of its triggers (ADR-0055 §2), driven
//! through the turn: what each answers of the requests a call holds or owes
//! a final for, and what it leaves to its real final.

use b2bua_core::rules::{Match, RuleAction, RuleDefinition, RuleHandleResult, SERVICE_LAYER};
use b2bua_core::turn_driver::{Driver, Session, Step, Turn};
use call::{CallModelState, Direction, TerminationCause};
use sip_message::Method;

use crate::calls::{bridged, ringing};

/// The service's topic: the call's termination begins.
const END: &str = "test-end";

/// A service holding every INFO bob sends for a later answer.
fn holds_bobs_info() -> RuleDefinition {
    RuleDefinition::core(
        "test-hold-info",
        SERVICE_LAYER,
        &[],
        Match::request().method("INFO").direction(Direction::FromB),
        |_| Some(RuleHandleResult::new(vec![RuleAction::HoldRequest])),
    )
}

/// A service holding every INFO either party sends for a later answer.
fn holds_every_info() -> RuleDefinition {
    RuleDefinition::core(
        "test-hold-every-info",
        SERVICE_LAYER,
        &[],
        Match::request().method("INFO"),
        |_| Some(RuleHandleResult::new(vec![RuleAction::HoldRequest])),
    )
}

/// The service's topic: bob's leg is split from the caller's.
const SPLIT: &str = "test-split";

/// A service splitting bob's leg from the caller's on its internal event.
fn splits_bob() -> RuleDefinition {
    RuleDefinition::core(
        "test-split",
        SERVICE_LAYER,
        &[],
        Match::internal_event().topic(SPLIT),
        |_| Some(RuleHandleResult::new(vec![RuleAction::Split { leg_id: "b-1".into() }])),
    )
}

/// The service's event splitting bob's leg.
fn splits() -> Step {
    Step::Internal { leg: "a", topic: SPLIT, outcome: "ok", payload: serde_json::json!({}) }
}

/// The held-answer timer's deadline on `call`, if armed.
fn held_answer_at(call: &Session<'_>) -> Option<i64> {
    let held = call::TimerKey::call(call::TimerType::HeldAnswer);
    call.call().timers.iter().find(|t| t.key == held).map(|t| t.fire_at)
}

/// The deadlines of the requests `call` still holds for a service.
fn held_deadlines(call: &Session<'_>) -> Vec<i64> {
    call::helpers::every_record(call.call())
        .filter(|(_, _, r)| r.is_held())
        .filter_map(|(_, _, r)| r.deadline_ms)
        .collect()
}

/// A service beginning the call's termination on its internal event.
fn ends_the_call() -> RuleDefinition {
    RuleDefinition::core("test-end", SERVICE_LAYER, &[], Match::internal_event().topic(END), |_| {
        Some(RuleHandleResult::new(vec![RuleAction::BeginTermination {
            reason: None,
            cause: TerminationCause::DecisionRelease,
            by_leg: None,
        }]))
    })
}

/// The service's event ending the call.
fn ends() -> Step {
    Step::Internal { leg: "a", topic: END, outcome: "ok", payload: serde_json::json!({}) }
}

/// The finals `turn` sent toward `leg`, `(status, method)`, in order.
fn finals_to(turn: &Turn, leg: &str) -> Vec<(u16, String)> {
    turn.responses_to(leg)
        .iter()
        .filter(|r| r.status() >= 200)
        .map(|r| (r.status(), r.cseq().method().as_str().to_string()))
        .collect()
}

/// `(status, method)` for a final.
fn fin(status: u16, method: &str) -> (u16, String) {
    (status, method.to_string())
}

/// What `call`'s legs still hold of non-own request records.
fn owed_left(call: &Session<'_>) -> usize {
    call::helpers::every_record(call.call()).filter(|(_, _, r)| !r.is_own()).count()
}

/// The termination begins on a bridged call: bob's INFO a service held is
/// answered 487, alice's re-INVITE still pending toward bob is answered 487
/// and CANCELled (RFC 3261 §15.1.2, §9.1). Her INFO relayed toward bob, a
/// Confirmed leg being BYEd, waits for its real final, which is relayed.
#[test]
fn the_termination_answers_what_is_held_and_the_pending_reinvite() {
    let driver = Driver::new().with_rule(holds_bobs_info()).with_rule(ends_the_call());
    let mut call = bridged(&driver);
    call.run([
        Step::Sends("a", Method::Info),
        Step::Sends("a", Method::Invite),
        Step::Sends("b-1", Method::Info),
    ]);
    assert!(finals_to(call.turns().last().unwrap(), "b-1").is_empty(), "bob's INFO is held");

    let turn = call.turned(ends());
    assert_eq!(finals_to(turn, "a"), [fin(487, "INVITE")], "only her re-INVITE is answered");
    assert_eq!(finals_to(turn, "b-1"), [fin(487, "INFO")], "the held INFO's 487");
    let to_bob: Vec<String> = turn.methods_to("b-1").into_iter().map(|(m, _)| m).collect();
    assert_eq!(to_bob, ["CANCEL", "BYE"], "the re-INVITE CANCELled, then the BYE");
    assert_eq!(call.call().state, CallModelState::Terminating);

    let turn = call.turned(Step::AnswersLast("b-1", Method::Info, 200));
    assert_eq!(finals_to(turn, "a"), [fin(200, "INFO")], "her INFO takes bob's own final");
}

/// The caller CANCELs a ringing call while bob's INFO relayed toward her
/// awaits its final: her INVITE carries the layer's 487, so the termination
/// settles her leg and bob's INFO is answered the method table's 481.
#[test]
fn the_callers_leg_is_settled_once_her_invite_carries_its_final() {
    let driver = Driver::new();
    let mut call = ringing(&driver);
    let relayed = call.turned(Step::Sends("b-1", Method::Info)).request_to("a", "INFO");
    assert!(relayed.is_some(), "bob's INFO is relayed to alice");

    let turn = call.turned(Step::AliceCancels);
    assert_eq!(finals_to(turn, "b-1"), [fin(481, "INFO")]);
    assert_eq!(owed_left(&call), 0);
}

/// A service ends a ringing call while alice's INFO relayed toward bob
/// awaits its final: his leg is CANCELled and settled, so her INFO is
/// answered 481; her own INVITE, unanswered, waits for the call's end.
#[test]
fn a_ringing_leg_is_settled_as_its_cancel_leaves() {
    let driver = Driver::new().with_rule(ends_the_call());
    let mut call = ringing(&driver);
    call.run([Step::Sends("a", Method::Info)]);

    let turn = call.turned(ends());
    assert_eq!(finals_to(turn, "a"), [fin(481, "INFO")], "her INFO, not yet her INVITE");
    assert_eq!(turn.methods_to("b-1").into_iter().map(|(m, _)| m).collect::<Vec<_>>(), ["CANCEL"]);
}

/// The caller's INVITE is still unanswered when the call ends: the ADR-0022
/// 503 leaves as it terminates, and bob's INFO relayed toward her is
/// answered 481 behind it.
#[test]
fn the_callers_leg_is_settled_behind_the_unanswered_callers_503() {
    let driver = Driver::new().with_rule(ends_the_call());
    let mut call = ringing(&driver);
    call.run([Step::Sends("b-1", Method::Info), ends(), Step::AnswersSent("b-1", 200)]);

    let turn = call.turned(Step::Answers("b-1", 487));
    assert_eq!(finals_to(turn, "a"), [fin(503, "INVITE")]);
    assert_eq!(finals_to(turn, "b-1"), [fin(481, "INFO")]);
    let labels: Vec<&str> = turn.result.effects.outbound.iter().map(|o| o.label.as_str()).collect();
    let at = |prefix: &str| labels.iter().position(|l| l.starts_with(prefix));
    let (her_503, his_481) = (at("503 "), at("481 INFO"));
    assert!(her_503.is_some() && her_503 < his_481, "her 503 leaves first: {labels:?}");
    assert_eq!(call.call().state, CallModelState::Terminated);
}

/// A service destroys bob's leg while alice's INFO relayed toward him
/// awaits its final: it is answered 481 as his leg ends.
#[test]
fn a_destroyed_leg_is_settled() {
    let driver = Driver::new().with_rule(crate::services::reinvite_release());
    let mut call = bridged(&driver);
    call.run([Step::Sends("a", Method::Info)]);

    let turn = call.turned(crate::services::releases("b-1"));
    assert_eq!(finals_to(turn, "a"), [fin(481, "INFO")]);
}

/// bob hangs up while alice's INFO relayed toward him awaits its final: the
/// rule ends his leg, which settles it, and her INFO is answered 481.
#[test]
fn a_leg_a_rule_ends_is_settled() {
    let driver = Driver::new();
    let mut call = bridged(&driver);
    call.run([Step::Sends("a", Method::Info)]);

    let turn = call.turned(Step::Sends("b-1", Method::Bye));
    assert!(finals_to(turn, "a").contains(&fin(481, "INFO")), "got {:?}", finals_to(turn, "a"));
}

/// bob refuses a ringing call no failover follows (its route carries no
/// callback context) while his INFO relayed toward alice awaits its final:
/// the call is released at once, every leg settled, and his INFO is answered
/// 481.
#[test]
fn a_call_released_at_once_settles_every_leg() {
    let driver = Driver::new().with_route(|r| r.callback_context = None);
    let mut call = ringing(&driver);
    call.run([Step::Sends("b-1", Method::Info)]);

    let turn = call.turned(Step::Answers("b-1", 486));
    assert_eq!(finals_to(turn, "b-1"), [fin(481, "INFO")]);
    assert_eq!(call.call().state, CallModelState::Terminated);
    assert_eq!(owed_left(&call), 0);
}

/// A request held for a service is answered when its leg ends, and the
/// held-answer timer leaves with the last one held.
#[test]
fn the_held_answer_timer_leaves_with_the_last_held_request() {
    let driver = Driver::new().with_rule(holds_bobs_info());
    let mut call = bridged(&driver);
    call.run([Step::Sends("b-1", Method::Info)]);
    let held = call::TimerKey::call(call::TimerType::HeldAnswer);
    assert!(call.call().timers.iter().any(|t| t.key == held), "armed while held");

    let turn = call.turned(Step::Sends("b-1", Method::Bye));
    assert!(finals_to(turn, "b-1").contains(&fin(487, "INFO")));
    assert!(!call.call().timers.iter().any(|t| t.key == held), "off once none is held");
}

/// A leg's end that answers the earliest held request, on a call that stays
/// up: the held-answer timer moves to the deadline of the request still
/// held.
#[test]
fn the_held_answer_timer_moves_to_the_request_still_held() {
    let driver =
        Driver::new().with_rule(holds_every_info()).with_rule(crate::services::reinvite_release());
    let mut call = bridged(&driver);
    call.run([
        Step::Sends("b-1", Method::Info),
        Step::Advance(1_000),
        Step::Sends("a", Method::Info),
    ]);
    let (bobs, hers) = match held_deadlines(&call)[..] {
        [x, y] => (x.min(y), x.max(y)),
        ref other => panic!("two requests held, got {other:?}"),
    };
    assert!(bobs < hers);
    assert_eq!(held_answer_at(&call), Some(bobs), "armed at the earliest");

    let turn = call.turned(crate::services::releases("b-1"));
    assert_eq!(finals_to(turn, "b-1"), [fin(487, "INFO")], "bob's held INFO");
    assert!(finals_to(turn, "a").is_empty(), "hers stays held");
    assert_ne!(call.call().state, CallModelState::Terminated);
    assert_eq!(held_answer_at(&call), Some(hers), "moved to the one still held");
}

/// A leg's end that answers the last held request, on a call that stays up:
/// the held-answer timer is off.
#[test]
fn the_held_answer_timer_is_off_once_none_is_held_on_a_live_call() {
    let driver =
        Driver::new().with_rule(holds_bobs_info()).with_rule(crate::services::reinvite_release());
    let mut call = bridged(&driver);
    call.run([Step::Sends("b-1", Method::Info)]);
    assert!(held_answer_at(&call).is_some(), "armed while held");

    let turn = call.turned(crate::services::releases("b-1"));
    assert_eq!(finals_to(turn, "b-1"), [fin(487, "INFO")]);
    assert_ne!(call.call().state, CallModelState::Terminated);
    assert_eq!(held_answer_at(&call), None, "off once none is held");
}

/// bob's re-INVITE awaits alice's final when a service splits his leg from
/// hers and then ends the call: the termination answers his re-INVITE 487
/// on his own leg, the one its final would have been relayed to.
#[test]
fn a_pending_reinvite_is_answered_on_the_leg_it_came_from_after_a_split() {
    let driver = Driver::new().with_rule(splits_bob()).with_rule(ends_the_call());
    let mut call = bridged(&driver);
    call.run([Step::Sends("b-1", Method::Invite), splits()]);
    assert!(call.call().active_peer.is_none(), "no active peer after the split");

    let turn = call.turned(ends());
    assert_eq!(finals_to(turn, "b-1"), [fin(487, "INVITE")]);
    assert!(finals_to(turn, "a").is_empty(), "nothing answered on her leg");
}
