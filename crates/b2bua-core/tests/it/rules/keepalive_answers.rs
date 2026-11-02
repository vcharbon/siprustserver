//! What an answer to the keepalive probe ends (RFC 3261 §11, §12.2.1.2; RFC
//! 5057 §5.1, §5.3): a dialog-terminating answer ends the call by the probed
//! leg under the keepalive deadline; any other refusal fails the probe alone;
//! a 481 is `handle-481`'s; a relayed OPTIONS's answer and a probe on a leg
//! already going away end nothing here.

use b2bua_core::turn_driver::{Driver, Step};
use call::TimeoutKind;
use sip_message::Method;

use super::*;

/// The rule that took bob's `status` to the probe sent on the bridged call,
/// and the call's termination after it.
fn probe_answered(status: u16) -> (Option<&'static str>, Option<call::Termination>) {
    let driver = Driver::new();
    let mut call = crate::calls::bridged(&driver);
    call.run([Step::Fires(TimerType::Keepalive), Step::AnswersLast("a", Method::Options, 200)]);
    let rule = call.turned(Step::AnswersLast("b-1", Method::Options, status)).rule;
    (rule, call.call().termination.clone())
}

#[test]
fn a_refusal_that_keeps_the_dialog_leaves_the_call_up() {
    for status in [405, 486, 500, 501, 503] {
        let (rule, termination) = probe_answered(status);
        assert_eq!(termination, None, "{status} fails the probe alone (rule {rule:?})");
    }
}

#[test]
fn a_dialog_terminating_answer_ends_the_call_by_the_probed_leg() {
    for status in [404, 408, 410, 416, 482, 483, 484, 485, 502, 604] {
        let (rule, termination) = probe_answered(status);
        assert_eq!(rule, Some("keepalive-dialog-terminated"), "{status}");
        let t = termination.expect("the call ends");
        assert_eq!(t.cause, TerminationCause::Timeout(TimeoutKind::Keepalive), "{status}");
        assert_eq!(t.by_leg.as_deref(), Some("b-1"), "{status}");
    }
}

#[test]
fn a_481_to_the_probe_is_handle_481s() {
    let (rule, termination) = probe_answered(481);
    assert_eq!(rule, Some("handle-481"));
    let cause = termination.expect("the call ends").cause;
    assert_eq!(cause, TerminationCause::Timeout(TimeoutKind::Keepalive));
}

/// The caller's own OPTIONS, relayed to bob and answered 404, is the
/// relayed exchange's: the 404 goes back to her and the call stays up.
#[test]
fn a_relayed_options_answered_404_ends_nothing() {
    let driver = Driver::new();
    let mut call = crate::calls::bridged(&driver);
    call.run([Step::Sends("a", Method::Options)]);
    let turn = call.turned(Step::AnswersLast("b-1", Method::Options, 404));
    assert_ne!(turn.rule, Some("keepalive-dialog-terminated"));
    assert_eq!(turn.statuses_to("a"), [404], "the refusal is relayed to the caller");
    assert_eq!(call.call().termination, None);
}

/// A probe answered 404 on a leg already going away (its BYE sent while the
/// call plays on) ends nothing more.
#[test]
fn a_probe_answered_on_a_leg_going_away_ends_nothing() {
    let driver = Driver::new();
    let mut call = crate::calls::bridged(&driver);
    call.run([Step::Fires(TimerType::Keepalive), Step::AnswersLast("a", Method::Options, 200)]);
    let mut record = call.call().clone();
    let leg = record.b_legs.iter_mut().find(|l| l.leg_id == "b-1").expect("bob's leg");
    call::testkit::stage_leg(leg, LegState::Terminated);
    call.revert_to(record);
    let turn = call.turned(Step::AnswersLast("b-1", Method::Options, 404));
    assert_ne!(turn.rule, Some("keepalive-dialog-terminated"));
    assert_eq!(call.call().termination, None);
}
