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
    assert_eq!(finals_to(turn, "b-1"), [fin(481, "INFO")], "after her 503");
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
